//! A standard interface for working with heterogeneous sources of requirements.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use configparser::ini::Ini;
use console::Term;
use distribution_filename::{SourceDistFilename, WheelFilename};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use rustc_hash::FxHashSet;
use serde::Deserialize;
use tracing::{debug, instrument, Level};

use distribution_types::{FlatIndexLocation, IndexUrl, RemoteSource};
use pep508_rs::{
    Requirement, RequirementsTxtRequirement, Scheme, UnnamedRequirement, VersionOrUrl,
};
use pypi_types::Metadata10;
use requirements_txt::{EditableRequirement, FindLink, RequirementsTxt};
use uv_client::Connectivity;
use uv_fs::Simplified;
use uv_normalize::{ExtraName, PackageName};
use uv_resolver::{Preference, PreferenceError};
use uv_warnings::warn_user;

use crate::commands::Upgrade;
use crate::confirm;

#[derive(Debug)]
pub(crate) enum RequirementsSource {
    /// A package was provided on the command line (e.g., `pip install flask`).
    Package(String),
    /// An editable path was provided on the command line (e.g., `pip install -e ../flask`).
    Editable(String),
    /// Dependencies were provided via a `requirements.txt` file (e.g., `pip install -r requirements.txt`).
    RequirementsTxt(PathBuf),
    /// Dependencies were provided via a `pyproject.toml` file (e.g., `pip-compile pyproject.toml`).
    PyprojectToml(PathBuf),
}

impl RequirementsSource {
    /// Parse a [`RequirementsSource`] from a [`PathBuf`].
    pub(crate) fn from_path(path: PathBuf) -> Self {
        if path.ends_with("pyproject.toml") {
            Self::PyprojectToml(path)
        } else {
            Self::RequirementsTxt(path)
        }
    }

    /// Parse a [`RequirementsSource`] from a user-provided string, assumed to be a package.
    ///
    /// If the user provided a value that appears to be a `requirements.txt` file or a local
    /// directory, prompt them to correct it (if the terminal is interactive).
    pub(crate) fn from_package(name: String) -> Self {
        // If the user provided a `requirements.txt` file without `-r` (as in
        // `uv pip install requirements.txt`), prompt them to correct it.
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        if (name.ends_with(".txt") || name.ends_with(".in")) && Path::new(&name).is_file() {
            let term = Term::stderr();
            if term.is_term() {
                let prompt = format!(
                    "`{name}` looks like a requirements file but was passed as a package name. Did you mean `-r {name}`?"
                );
                let confirmation = confirm::confirm(&prompt, &term, true).unwrap();
                if confirmation {
                    return Self::RequirementsTxt(name.into());
                }
            }
        }

        Self::Package(name)
    }
}

impl std::fmt::Display for RequirementsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Editable(path) => write!(f, "-e {path}"),
            Self::RequirementsTxt(path) | Self::PyprojectToml(path) => {
                write!(f, "{}", path.display())
            }
            Self::Package(package) => write!(f, "{package}"),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) enum ExtrasSpecification<'a> {
    #[default]
    None,
    All,
    Some(&'a [ExtraName]),
}

impl ExtrasSpecification<'_> {
    /// Returns true if a name is included in the extra specification.
    fn contains(&self, name: &ExtraName) -> bool {
        match self {
            ExtrasSpecification::All => true,
            ExtrasSpecification::None => false,
            ExtrasSpecification::Some(extras) => extras.contains(name),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct RequirementsSpecification {
    /// The name of the project specifying requirements.
    pub(crate) project: Option<PackageName>,
    /// The requirements for the project.
    pub(crate) requirements: Vec<RequirementsTxtRequirement>,
    /// The constraints for the project.
    pub(crate) constraints: Vec<Requirement>,
    /// The overrides for the project.
    pub(crate) overrides: Vec<Requirement>,
    /// Package to install as editable installs
    pub(crate) editables: Vec<EditableRequirement>,
    /// The extras used to collect requirements.
    pub(crate) extras: FxHashSet<ExtraName>,
    /// The index URL to use for fetching packages.
    pub(crate) index_url: Option<IndexUrl>,
    /// The extra index URLs to use for fetching packages.
    pub(crate) extra_index_urls: Vec<IndexUrl>,
    /// Whether to disallow index usage.
    pub(crate) no_index: bool,
    /// The `--find-links` locations to use for fetching packages.
    pub(crate) find_links: Vec<FlatIndexLocation>,
}

impl RequirementsSpecification {
    /// Read the requirements and constraints from a source.
    #[instrument(skip_all, level = Level::DEBUG, fields(source = % source))]
    pub(crate) async fn from_source(
        source: &RequirementsSource,
        extras: &ExtrasSpecification<'_>,
        connectivity: Connectivity,
    ) -> Result<Self> {
        Ok(match source {
            RequirementsSource::Package(name) => {
                let requirement = RequirementsTxtRequirement::parse(name, std::env::current_dir()?)
                    .with_context(|| format!("Failed to parse `{name}`"))?;
                Self {
                    project: None,
                    requirements: vec![requirement],
                    constraints: vec![],
                    overrides: vec![],
                    editables: vec![],
                    extras: FxHashSet::default(),
                    index_url: None,
                    extra_index_urls: vec![],
                    no_index: false,
                    find_links: vec![],
                }
            }
            RequirementsSource::Editable(name) => {
                let requirement = EditableRequirement::parse(name, std::env::current_dir()?)
                    .with_context(|| format!("Failed to parse `{name}`"))?;
                Self {
                    project: None,
                    requirements: vec![],
                    constraints: vec![],
                    overrides: vec![],
                    editables: vec![requirement],
                    extras: FxHashSet::default(),
                    index_url: None,
                    extra_index_urls: vec![],
                    no_index: false,
                    find_links: vec![],
                }
            }
            RequirementsSource::RequirementsTxt(path) => {
                let requirements_txt =
                    RequirementsTxt::parse(path, std::env::current_dir()?, connectivity).await?;
                Self {
                    project: None,
                    requirements: requirements_txt
                        .requirements
                        .into_iter()
                        .map(|entry| entry.requirement)
                        .collect(),
                    constraints: requirements_txt.constraints,
                    editables: requirements_txt.editables,
                    overrides: vec![],
                    extras: FxHashSet::default(),
                    index_url: requirements_txt.index_url.map(IndexUrl::from),
                    extra_index_urls: requirements_txt
                        .extra_index_urls
                        .into_iter()
                        .map(IndexUrl::from)
                        .collect(),
                    no_index: requirements_txt.no_index,
                    find_links: requirements_txt
                        .find_links
                        .into_iter()
                        .map(|link| match link {
                            FindLink::Url(url) => FlatIndexLocation::Url(url),
                            FindLink::Path(path) => FlatIndexLocation::Path(path),
                        })
                        .collect(),
                }
            }
            RequirementsSource::PyprojectToml(path) => {
                let contents = uv_fs::read_to_string(path).await?;
                let pyproject_toml = toml::from_str::<pyproject_toml::PyProjectToml>(&contents)
                    .with_context(|| format!("Failed to parse `{}`", path.user_display()))?;
                let mut used_extras = FxHashSet::default();
                let mut requirements = Vec::new();
                let mut project_name = None;

                if let Some(project) = pyproject_toml.project {
                    // Parse the project name.
                    let parsed_project_name =
                        PackageName::new(project.name).with_context(|| {
                            format!("Invalid `project.name` in {}", path.user_display())
                        })?;

                    // Include the default dependencies.
                    requirements.extend(project.dependencies.unwrap_or_default());

                    // Include any optional dependencies specified in `extras`.
                    if !matches!(extras, ExtrasSpecification::None) {
                        if let Some(optional_dependencies) = project.optional_dependencies {
                            for (extra_name, optional_requirements) in &optional_dependencies {
                                // TODO(konstin): It's not ideal that pyproject-toml doesn't use
                                // `ExtraName`
                                let normalized_name = ExtraName::from_str(extra_name)?;
                                if extras.contains(&normalized_name) {
                                    used_extras.insert(normalized_name);
                                    requirements.extend(flatten_extra(
                                        &parsed_project_name,
                                        optional_requirements,
                                        &optional_dependencies,
                                    )?);
                                }
                            }
                        }
                    }

                    project_name = Some(parsed_project_name);
                }

                if requirements.is_empty()
                    && pyproject_toml.build_system.is_some_and(|build_system| {
                        build_system
                            .requires
                            .iter()
                            .any(|v| v.name.as_dist_info_name().starts_with("poetry"))
                    })
                {
                    warn_user!("`{}` does not contain any dependencies (hint: specify dependencies in the `project.dependencies` section; `tool.poetry.dependencies` is not currently supported)", path.user_display());
                }

                Self {
                    project: project_name,
                    requirements: requirements
                        .into_iter()
                        .map(RequirementsTxtRequirement::Pep508)
                        .collect(),
                    constraints: vec![],
                    overrides: vec![],
                    editables: vec![],
                    extras: used_extras,
                    index_url: None,
                    extra_index_urls: vec![],
                    no_index: false,
                    find_links: vec![],
                }
            }
        })
    }

    /// Read the combined requirements and constraints from a set of sources.
    pub(crate) async fn from_sources(
        requirements: &[RequirementsSource],
        constraints: &[RequirementsSource],
        overrides: &[RequirementsSource],
        extras: &ExtrasSpecification<'_>,
        connectivity: Connectivity,
    ) -> Result<Self> {
        let mut spec = Self::default();

        // Read all requirements, and keep track of all requirements _and_ constraints.
        // A `requirements.txt` can contain a `-c constraints.txt` directive within it, so reading
        // a requirements file can also add constraints.
        for source in requirements {
            let source = Self::from_source(source, extras, connectivity).await?;
            spec.requirements.extend(source.requirements);
            spec.constraints.extend(source.constraints);
            spec.overrides.extend(source.overrides);
            spec.extras.extend(source.extras);
            spec.editables.extend(source.editables);

            // Use the first project name discovered.
            if spec.project.is_none() {
                spec.project = source.project;
            }

            if let Some(url) = source.index_url {
                if let Some(existing) = spec.index_url {
                    return Err(anyhow::anyhow!(
                        "Multiple index URLs specified: `{existing}` vs.` {url}",
                    ));
                }
                spec.index_url = Some(url);
            }
            spec.no_index |= source.no_index;
            spec.extra_index_urls.extend(source.extra_index_urls);
            spec.find_links.extend(source.find_links);
        }

        // Read all constraints, treating _everything_ as a constraint.
        for source in constraints {
            let source = Self::from_source(source, extras, connectivity).await?;
            for requirement in source.requirements {
                match requirement {
                    RequirementsTxtRequirement::Pep508(requirement) => {
                        spec.constraints.push(requirement);
                    }
                    RequirementsTxtRequirement::Unnamed(requirement) => {
                        return Err(anyhow::anyhow!(
                            "Unnamed requirements are not allowed as constraints (found: `{requirement}`)"
                        ));
                    }
                }
            }
            spec.constraints.extend(source.constraints);
            spec.constraints.extend(source.overrides);

            if let Some(url) = source.index_url {
                if let Some(existing) = spec.index_url {
                    return Err(anyhow::anyhow!(
                        "Multiple index URLs specified: `{existing}` vs.` {url}",
                    ));
                }
                spec.index_url = Some(url);
            }
            spec.no_index |= source.no_index;
            spec.extra_index_urls.extend(source.extra_index_urls);
            spec.find_links.extend(source.find_links);
        }

        // Read all overrides, treating both requirements _and_ constraints as overrides.
        for source in overrides {
            let source = Self::from_source(source, extras, connectivity).await?;
            for requirement in source.requirements {
                match requirement {
                    RequirementsTxtRequirement::Pep508(requirement) => {
                        spec.overrides.push(requirement);
                    }
                    RequirementsTxtRequirement::Unnamed(requirement) => {
                        return Err(anyhow::anyhow!(
                            "Unnamed requirements are not allowed as overrides (found: `{requirement}`)"
                        ));
                    }
                }
            }
            spec.overrides.extend(source.constraints);
            spec.overrides.extend(source.overrides);

            if let Some(url) = source.index_url {
                if let Some(existing) = spec.index_url {
                    return Err(anyhow::anyhow!(
                        "Multiple index URLs specified: `{existing}` vs.` {url}",
                    ));
                }
                spec.index_url = Some(url);
            }
            spec.no_index |= source.no_index;
            spec.extra_index_urls.extend(source.extra_index_urls);
            spec.find_links.extend(source.find_links);
        }

        Ok(spec)
    }

    /// Read the requirements from a set of sources.
    pub(crate) async fn from_simple_sources(
        requirements: &[RequirementsSource],
        connectivity: Connectivity,
    ) -> Result<Self> {
        Self::from_sources(
            requirements,
            &[],
            &[],
            &ExtrasSpecification::None,
            connectivity,
        )
        .await
    }
}

/// Given an extra in a project that may contain references to the project
/// itself, flatten it into a list of requirements.
///
/// For example:
/// ```toml
/// [project]
/// name = "my-project"
/// version = "0.0.1"
/// dependencies = [
///     "tomli",
/// ]
///
/// [project.optional-dependencies]
/// test = [
///     "pep517",
/// ]
/// dev = [
///     "my-project[test]",
/// ]
/// ```
fn flatten_extra(
    project_name: &PackageName,
    requirements: &[Requirement],
    extras: &IndexMap<String, Vec<Requirement>>,
) -> Result<Vec<Requirement>> {
    fn inner(
        project_name: &PackageName,
        requirements: &[Requirement],
        extras: &IndexMap<String, Vec<Requirement>>,
        seen: &mut FxHashSet<ExtraName>,
    ) -> Result<Vec<Requirement>> {
        let mut flattened = Vec::with_capacity(requirements.len());
        for requirement in requirements {
            if requirement.name == *project_name {
                for extra in &requirement.extras {
                    // Avoid infinite recursion on mutually recursive extras.
                    if !seen.insert(extra.clone()) {
                        continue;
                    }

                    // Flatten the extra requirements.
                    for (name, extra_requirements) in extras {
                        let normalized_name = ExtraName::from_str(name)?;
                        if normalized_name == *extra {
                            flattened.extend(inner(
                                project_name,
                                extra_requirements,
                                extras,
                                seen,
                            )?);
                        }
                    }
                }
            } else {
                flattened.push(requirement.clone());
            }
        }
        Ok(flattened)
    }

    inner(
        project_name,
        requirements,
        extras,
        &mut FxHashSet::default(),
    )
}

/// Load the preferred requirements from an existing lockfile, applying the upgrade strategy.
pub(crate) async fn read_lockfile(
    output_file: Option<&Path>,
    upgrade: Upgrade,
) -> Result<Vec<Preference>> {
    // As an optimization, skip reading the lockfile is we're upgrading all packages anyway.
    let Some(output_file) = output_file
        .filter(|_| !upgrade.is_all())
        .filter(|output_file| output_file.exists())
    else {
        return Ok(Vec::new());
    };

    // Parse the requirements from the lockfile.
    let requirements_txt =
        RequirementsTxt::parse(output_file, std::env::current_dir()?, Connectivity::Offline)
            .await?;
    let preferences = requirements_txt
        .requirements
        .into_iter()
        .filter(|entry| !entry.editable)
        .map(Preference::from_entry)
        .collect::<Result<Vec<_>, PreferenceError>>()?;

    // Apply the upgrade strategy to the requirements.
    Ok(match upgrade {
        // Respect all pinned versions from the existing lockfile.
        Upgrade::None => preferences,
        // Ignore all pinned versions from the existing lockfile.
        Upgrade::All => vec![],
        // Ignore pinned versions for the specified packages.
        Upgrade::Packages(packages) => preferences
            .into_iter()
            .filter(|preference| !packages.contains(preference.name()))
            .collect(),
    })
}

/// Like [`RequirementsSpecification`], but with concrete names for all requirements.
#[derive(Debug, Default)]
pub(crate) struct NamedRequirements {
    /// The name of the project specifying requirements.
    pub(crate) project: Option<PackageName>,
    /// The requirements for the project.
    pub(crate) requirements: Vec<Requirement>,
    /// The constraints for the project.
    pub(crate) constraints: Vec<Requirement>,
    /// The overrides for the project.
    pub(crate) overrides: Vec<Requirement>,
    /// Package to install as editable installs
    pub(crate) editables: Vec<EditableRequirement>,
    /// The index URL to use for fetching packages.
    pub(crate) index_url: Option<IndexUrl>,
    /// The extra index URLs to use for fetching packages.
    pub(crate) extra_index_urls: Vec<IndexUrl>,
    /// Whether to disallow index usage.
    pub(crate) no_index: bool,
    /// The `--find-links` locations to use for fetching packages.
    pub(crate) find_links: Vec<FlatIndexLocation>,
}

impl NamedRequirements {
    /// Convert a [`RequirementsSpecification`] into a [`NamedRequirements`].
    pub(crate) fn from_spec(spec: RequirementsSpecification) -> Result<Self> {
        Ok(Self {
            project: spec.project,
            requirements: spec
                .requirements
                .into_iter()
                .map(|requirement| match requirement {
                    RequirementsTxtRequirement::Pep508(requirement) => Ok(requirement),
                    RequirementsTxtRequirement::Unnamed(requirement) => {
                        Self::name_requirement(requirement)
                    }
                })
                .collect::<Result<_>>()?,
            constraints: spec.constraints,
            overrides: spec.overrides,
            editables: spec.editables,
            index_url: spec.index_url,
            extra_index_urls: spec.extra_index_urls,
            no_index: spec.no_index,
            find_links: spec.find_links,
        })
    }

    /// Infer the package name for a given "unnamed" requirement.
    fn name_requirement(requirement: UnnamedRequirement) -> Result<Requirement> {
        // If the requirement is a wheel, extract the package name from the wheel filename.
        //
        // Ex) `anyio-4.3.0-py3-none-any.whl`
        if Path::new(requirement.url.path())
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
        {
            let filename = WheelFilename::from_str(&requirement.url.filename()?)?;
            return Ok(Requirement {
                name: filename.name,
                extras: requirement.extras,
                version_or_url: Some(VersionOrUrl::Url(requirement.url)),
                marker: requirement.marker,
            });
        }

        // If the requirement is a source archive, try to extract the package name from the archive
        // filename. This isn't guaranteed to work.
        //
        // Ex) `anyio-4.3.0.tar.gz`
        if let Some(filename) = requirement
            .url
            .filename()
            .ok()
            .and_then(|filename| SourceDistFilename::parsed_normalized_filename(&filename).ok())
        {
            return Ok(Requirement {
                name: filename.name,
                extras: requirement.extras,
                version_or_url: Some(VersionOrUrl::Url(requirement.url)),
                marker: requirement.marker,
            });
        }

        // Otherwise, download and/or extract the source archive.
        if Scheme::parse(requirement.url.scheme()) == Some(Scheme::File) {
            let path = requirement.url.to_file_path().map_err(|()| {
                anyhow::anyhow!("Unable to convert file URL to path: {requirement}")
            })?;

            if !path.exists() {
                return Err(anyhow::anyhow!(
                    "Unnamed requirement at {path} not found",
                    path = path.simplified_display()
                ));
            }

            // Attempt to read a `PKG-INFO` from the directory.
            if let Some(metadata) = fs_err::read(path.join("PKG-INFO"))
                .ok()
                .and_then(|contents| Metadata10::parse_pkg_info(&contents).ok())
            {
                debug!(
                    "Found PKG-INFO metadata for {path} ({name})",
                    path = path.display(),
                    name = metadata.name
                );
                return Ok(Requirement {
                    name: metadata.name,
                    extras: requirement.extras,
                    version_or_url: Some(VersionOrUrl::Url(requirement.url)),
                    marker: requirement.marker,
                });
            }

            // Attempt to read a `pyproject.toml` file.
            if let Some(pyproject) = fs_err::read_to_string(path.join("pyproject.toml"))
                .ok()
                .and_then(|contents| toml::from_str::<PyProjectToml>(&contents).ok())
            {
                // Read PEP 621 metadata from the `pyproject.toml`.
                if let Some(project) = pyproject.project {
                    debug!(
                        "Found PEP 621 metadata for {path} in `pyproject.toml` ({name})",
                        path = path.display(),
                        name = project.name
                    );
                    return Ok(Requirement {
                        name: project.name,
                        extras: requirement.extras,
                        version_or_url: Some(VersionOrUrl::Url(requirement.url)),
                        marker: requirement.marker,
                    });
                }

                // Read Poetry-specific metadata from the `pyproject.toml`.
                if let Some(tool) = pyproject.tool {
                    if let Some(poetry) = tool.poetry {
                        if let Some(name) = poetry.name {
                            debug!(
                                "Found Poetry metadata for {path} in `pyproject.toml` ({name})",
                                path = path.display(),
                                name = name
                            );
                            return Ok(Requirement {
                                name,
                                extras: requirement.extras,
                                version_or_url: Some(VersionOrUrl::Url(requirement.url)),
                                marker: requirement.marker,
                            });
                        }
                    }
                }
            }

            // Attempt to read a `setup.cfg` from the directory.
            if let Some(setup_cfg) = fs_err::read_to_string(path.join("setup.cfg"))
                .ok()
                .and_then(|contents| {
                    let mut ini = Ini::new_cs();
                    ini.set_multiline(true);
                    ini.read(contents).ok()
                })
            {
                if let Some(section) = setup_cfg.get("metadata") {
                    if let Some(Some(name)) = section.get("name") {
                        if let Ok(name) = PackageName::from_str(name) {
                            debug!(
                                "Found setuptools metadata for {path} in `setup.cfg` ({name})",
                                path = path.display(),
                                name = name
                            );
                            return Ok(Requirement {
                                name,
                                extras: requirement.extras,
                                version_or_url: Some(VersionOrUrl::Url(requirement.url)),
                                marker: requirement.marker,
                            });
                        }
                    }
                }
            }

            // Attempt to read a `setup.py` from the directory.
            if let Ok(setup_py) = fs_err::read_to_string(path.join("setup.py")) {
                static SETUP_PY_NAME: Lazy<Regex> =
                    Lazy::new(|| Regex::new(r#"name\s*[=:]\s*['"](?P<name>[^'"]+)['"]"#).unwrap());

                if let Some(name) = SETUP_PY_NAME
                    .captures(&setup_py)
                    .and_then(|captures| captures.name("name"))
                    .map(|name| name.as_str())
                {
                    if let Ok(name) = PackageName::from_str(name) {
                        debug!(
                            "Found setuptools metadata for {path} in `setup.py` ({name})",
                            path = path.display(),
                            name = name
                        );
                        return Ok(Requirement {
                            name,
                            extras: requirement.extras,
                            version_or_url: Some(VersionOrUrl::Url(requirement.url)),
                            marker: requirement.marker,
                        });
                    }
                }
            }
        }

        Err(anyhow::anyhow!(
            "Unable to infer package name for the unnamed requirement: {requirement}"
        ))
    }
}

/// A pyproject.toml as specified in PEP 517.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct PyProjectToml {
    project: Option<Project>,
    tool: Option<Tool>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct Project {
    name: PackageName,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct Tool {
    poetry: Option<ToolPoetry>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct ToolPoetry {
    name: Option<PackageName>,
}
