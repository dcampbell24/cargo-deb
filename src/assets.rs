use crate::compress;
use crate::debian_architecture_from_rust_triple;
use crate::dependencies::resolve;
use crate::dh::dh_installsystemd;
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::parse::config::CargoConfig;
use crate::parse::manifest::{
    cargo_metadata, manifest_debug_flag, manifest_license_file, manifest_version_string,
};
use crate::parse::manifest::{
    CargoDeb, CargoMetadataTarget, CargoPackageMetadata, ManifestFound,
};
use crate::parse::manifest::{DependencyList, SystemUnitsSingleOrMultiple, SystemdUnitsConfig};
use crate::util::ok_or::OkOrThen;
use crate::util::pathbytes::AsUnixPathBytes;
use crate::util::read_file_to_bytes;
use crate::util::wordsplit::WordSplit;
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env::consts::{DLL_PREFIX, DLL_SUFFIX, EXE_SUFFIX};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn is_glob_pattern(s: &Path) -> bool {
    s.to_bytes().iter().any(|&c| c == b'*' || c == b'[' || c == b']' || c == b'!')
}

#[derive(Debug, Clone)]
pub enum AssetSource {
    /// Copy file from the path (and strip binary if needed).
    Path(PathBuf),
    /// A symlink existing in the file system
    Symlink(PathBuf),
    /// Write data to destination as-is.
    Data(Vec<u8>),
}

impl AssetSource {
    /// Symlink must exist on disk to be preserved
    #[must_use]
    pub fn from_path(path: impl Into<PathBuf>, preserve_existing_symlink: bool) -> Self {
        let path = path.into();
        if preserve_existing_symlink || !path.exists() { // !exists means a symlink to bogus path
            if let Ok(md) = fs::symlink_metadata(&path) {
                if md.is_symlink() {
                    return Self::Symlink(path);
                }
            }
        }
        Self::Path(path)
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            AssetSource::Symlink(ref p) |
            AssetSource::Path(ref p) => Some(p),
            AssetSource::Data(_) => None,
        }
    }

    #[must_use]
    pub fn into_path(self) -> Option<PathBuf> {
        match self {
            AssetSource::Symlink(p) |
            AssetSource::Path(p) => Some(p),
            AssetSource::Data(_) => None,
        }
    }

    #[must_use]
    pub fn archive_as_symlink_only(&self) -> bool {
        matches!(self, AssetSource::Symlink(_))
    }

    #[must_use]
    pub fn file_size(&self) -> Option<u64> {
        match *self {
            // FIXME: may not be accurate if the executable is not stripped yet?
            AssetSource::Path(ref p) => fs::metadata(p).ok().map(|m| m.len()),
            AssetSource::Data(ref d) => Some(d.len() as u64),
            AssetSource::Symlink(_) => None,
        }
    }

    pub fn data(&self) -> CDResult<Cow<'_, [u8]>> {
        Ok(match self {
            AssetSource::Path(p) => {
                let data = read_file_to_bytes(p)
                    .map_err(|e| CargoDebError::IoFile("unable to read asset to add to archive", e, p.clone()))?;
                Cow::Owned(data)
            },
            AssetSource::Data(d) => Cow::Borrowed(d),
            AssetSource::Symlink(_) => return Err(CargoDebError::Str("Symlink unexpectedly used to read file data")),
        })
    }
}

/// Match the official `dh_installsystemd` defaults and rename the confusing
/// `dh_installsystemd` option names to be consistently positive rather than
/// mostly, but not always, negative.
impl From<&SystemdUnitsConfig> for dh_installsystemd::Options {
    fn from(config: &SystemdUnitsConfig) -> Self {
        Self {
            no_enable: !config.enable.unwrap_or(true),
            no_start: !config.start.unwrap_or(true),
            restart_after_upgrade: config.restart_after_upgrade.unwrap_or(true),
            no_stop_on_upgrade: !config.stop_on_upgrade.unwrap_or(true),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Assets {
    pub unresolved: Vec<UnresolvedAsset>,
    pub resolved: Vec<Asset>,
}

impl Assets {
    fn new() -> Assets {
        Assets {
            unresolved: vec![],
            resolved: vec![],
        }
    }

    fn with_resolved_assets(assets: Vec<Asset>) -> Assets {
        Assets {
            unresolved: vec![],
            resolved: assets,
        }
    }

    fn with_unresolved_assets(assets: Vec<UnresolvedAsset>) -> Assets {
        Assets {
            unresolved: assets,
            resolved: vec![],
        }
    }

    fn is_empty(&self) -> bool {
        self.unresolved.is_empty() && self.resolved.is_empty()
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IsBuilt {
    No,
    SamePackage,
    /// needs --workspace to build
    Workspace,
}

#[derive(Debug, Clone)]
pub struct UnresolvedAsset {
    pub source_path: PathBuf,
    pub c: AssetCommon,
}

#[derive(Debug, Clone)]
pub struct AssetCommon {
    pub target_path: PathBuf,
    pub chmod: u32,
    is_built: IsBuilt,
    is_example: bool,
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub source: AssetSource,
    /// For prettier path display not "/tmp/blah.tmp"
    pub processed_from: Option<ProcessedFrom>,
    pub c: AssetCommon,
}

#[derive(Debug, Clone)]
pub struct ProcessedFrom {
    pub original_path: Option<PathBuf>,
    pub action: &'static str,
}

impl Asset {
    #[must_use]
    pub fn new(source: AssetSource, mut target_path: PathBuf, chmod: u32, is_built: IsBuilt, is_example: bool) -> Self {
        // is_dir() is only for paths that exist
        if target_path.to_string_lossy().ends_with('/') {
            let file_name = source.path().and_then(|p| p.file_name()).expect("source must be a file");
            target_path = target_path.join(file_name);
        }

        if target_path.is_absolute() || target_path.has_root() {
            target_path = target_path.strip_prefix("/").expect("no root dir").to_owned();
        }

        Self {
            source,
            processed_from: None,
            c: AssetCommon {
                target_path, chmod, is_built, is_example,
            },
        }
    }

    #[must_use]
    pub fn processed(mut self, action: &'static str, original_path: impl Into<Option<PathBuf>>) -> Self {
        debug_assert!(self.processed_from.is_none());
        self.processed_from = Some(ProcessedFrom {
            original_path: original_path.into(),
            action,
        });
        self
    }
}

impl AssetCommon {
    fn is_executable(&self) -> bool {
        0 != self.chmod & 0o111
    }

    fn is_dynamic_library(&self) -> bool {
        is_dynamic_library_filename(&self.target_path)
    }

    pub(crate) fn is_built(&self) -> bool {
        self.is_built != IsBuilt::No
    }

    /// Returns the target path for the debug symbol file, which will be
    /// /usr/lib/debug/<path-to-executable>.debug
    #[must_use]
    pub(crate) fn default_debug_target_path(&self) -> PathBuf {
        // Turn an absolute path into one relative to "/"
        let relative = self.target_path.strip_prefix(Path::new("/"))
            .unwrap_or(self.target_path.as_path());

        // Prepend the debug location
        let debug_path = Path::new("/usr/lib/debug").join(relative);

        // Add `.debug` to the end of the filename
        debug_filename(&debug_path)
    }
}

/// Adds `.debug` to the end of a path to a filename
///
fn debug_filename(path: &Path) -> PathBuf {
    let mut debug_filename = path.as_os_str().to_os_string();
    debug_filename.push(".debug");
    debug_filename.into()
}

fn is_dynamic_library_filename(path: &Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .map_or(false, |f| f.ends_with(DLL_SUFFIX))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ArchSpec {
    /// e.g. [armhf]
    Require(String),
    /// e.g. [!armhf]
    NegRequire(String),
}

fn get_architecture_specification(depend: &str) -> CDResult<(String, Option<ArchSpec>)> {
    use ArchSpec::{NegRequire, Require};
    let re = regex::Regex::new(r#"(.*)\[(!?)(.*)\]"#).unwrap();
    match re.captures(depend) {
        Some(caps) => {
            let spec = if &caps[2] == "!" {
                NegRequire(caps[3].to_string())
            } else {
                assert_eq!(&caps[2], "");
                Require(caps[3].to_string())
            };
            Ok((caps[1].trim().to_string(), Some(spec)))
        },
        None => Ok((depend.to_string(), None)),
    }
}

/// Architecture specification strings
/// <https://www.debian.org/doc/debian-policy/ch-customized-programs.html#s-arch-spec>
fn match_architecture(spec: ArchSpec, target_arch: &str) -> CDResult<bool> {
    let (neg, spec) = match spec {
        ArchSpec::NegRequire(pkg) => (true, pkg),
        ArchSpec::Require(pkg) => (false, pkg),
    };
    let output = Command::new("dpkg-architecture")
        .args(["-a", target_arch, "-i", &spec])
        .output()
        .map_err(|e| CargoDebError::CommandFailed(e, "dpkg-architecture"))?;
    if neg {
        Ok(!output.status.success())
    } else {
        Ok(output.status.success())
    }
}

#[derive(Debug)]
#[non_exhaustive]
/// Cargo deb configuration read from the manifest and cargo metadata
pub struct Config {
    /// Directory where `Cargo.toml` is located. It's a subdirectory in workspaces.
    pub package_manifest_dir: PathBuf,
    /// User-configured output path for *.deb
    pub deb_output_path: Option<String>,
    /// Triple. `None` means current machine architecture.
    pub target: Option<String>,
    /// `CARGO_TARGET_DIR`
    pub target_dir: PathBuf,
    /// List of Cargo features to use during build
    pub features: Vec<String>,
    pub default_features: bool,
    /// Should the binary be stripped from debug symbols?
    pub debug_symbols: DebugSymbols,
    pub deb: Package,
}

#[derive(Debug)]
#[non_exhaustive]
pub struct Package {
    /// The name of the project to build
    pub name: String,
    /// The name to give the Debian package; usually the same as the Cargo project name
    pub deb_name: String,
    /// The version to give the Debian package; usually the same as the Cargo version
    pub deb_version: String,
    /// The software license of the project (SPDX format).
    pub license: Option<String>,
    /// The location of the license file
    pub license_file: Option<PathBuf>,
    /// number of lines to skip when reading `license_file`
    pub license_file_skip_lines: usize,
    /// The copyright of the project
    /// (Debian's `copyright` file contents).
    pub copyright: String,
    pub changelog: Option<String>,
    /// The homepage URL of the project.
    pub homepage: Option<String>,
    /// Documentation URL from `Cargo.toml`. Fallback if `homepage` is missing.
    pub documentation: Option<String>,
    /// The URL of the software repository.
    pub repository: Option<String>,
    /// A short description of the project.
    pub description: String,
    /// An extended description of the project.
    pub extended_description: Option<String>,
    /// The maintainer of the Debian package.
    /// In Debian `control` file `Maintainer` field format.
    pub maintainer: String,
    /// The Debian dependencies required to run the project.
    pub depends: String,
    /// The Debian pre-dependencies.
    pub pre_depends: Option<String>,
    /// The Debian recommended dependencies.
    pub recommends: Option<String>,
    /// The Debian suggested dependencies.
    pub suggests: Option<String>,
    /// The list of packages this package can enhance.
    pub enhances: Option<String>,
    /// The Debian software category to which the package belongs.
    pub section: Option<String>,
    /// The Debian priority of the project. Typically 'optional'.
    pub priority: String,

    /// `Conflicts` Debian control field.
    ///
    /// See [PackageTransition](https://wiki.debian.org/PackageTransition).
    pub conflicts: Option<String>,
    /// `Breaks` Debian control field.
    ///
    /// See [PackageTransition](https://wiki.debian.org/PackageTransition).
    pub breaks: Option<String>,
    /// `Replaces` Debian control field.
    ///
    /// See [PackageTransition](https://wiki.debian.org/PackageTransition).
    pub replaces: Option<String>,
    /// `Provides` Debian control field.
    ///
    /// See [PackageTransition](https://wiki.debian.org/PackageTransition).
    pub provides: Option<String>,

    /// The Debian architecture of the target system.
    pub architecture: String,
    /// A list of configuration files installed by the package.
    pub conf_files: Option<String>,
    /// All of the files that are to be packaged.
    pub(crate) assets: Assets,
    /// The location of the triggers file
    pub triggers_file: Option<PathBuf>,
    /// The path where possible maintainer scripts live
    pub maintainer_scripts: Option<PathBuf>,
    /// Should symlinks be preserved in the assets
    pub preserve_symlinks: bool,
    /// Details of how to install any systemd units
    pub(crate) systemd_units: Option<Vec<SystemdUnitsConfig>>,
    /// unix timestamp for generated files
    pub default_timestamp: u64,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum DebugSymbols {
    Keep,
    Strip,
    /// Should the debug symbols be moved to a separate file included in the package? (implies `strip:true`)
    Separate {
        /// Should the debug symbols be compressed
        compress: bool,
    },
}

impl Config {
    /// Makes a new config from `Cargo.toml` in the `manifest_path`
    ///
    /// `None` target means the host machine's architecture.
    pub fn from_manifest(
        root_manifest_path: Option<&Path>,
        selected_package_name: Option<&str>,
        deb_output_path: Option<String>,
        target: Option<&str>,
        variant: Option<&str>,
        deb_version: Option<String>,
        deb_revision: Option<String>,
        listener: &dyn Listener,
        selected_profile: &str,
        separate_debug_symbols: Option<bool>,
        compress_debug_symbols: Option<bool>,
    ) -> CDResult<Self> {
        // **IMPORTANT**: This function must not create or expect to see any files on disk!
        // It's run before destination directory is cleaned up, and before the build start!

        let ManifestFound {
            targets,
            root_manifest,
            mut manifest_path,
            mut target_dir,
            mut manifest,
            default_timestamp,
        } = cargo_metadata(root_manifest_path, selected_package_name)?;
        manifest_path.pop();
        let package_manifest_dir = manifest_path;

        // Cargo cross-compiles to a dir
        if let Some(target) = target {
            target_dir.push(target);
        };

        // FIXME: support other named profiles
        let debug_enabled = if selected_profile == "release" {
            manifest_debug_flag(&manifest) || root_manifest.as_ref().map_or(false, manifest_debug_flag)
        } else {
            false
        };
        drop(root_manifest);
        let package = manifest.package.as_mut().ok_or("bad package")?;

        // If we build against a variant use that config and change the package name
        let mut deb = if let Some(variant) = variant {
            // Use dash as underscore is not allowed in package names
            package.name = format!("{}-{variant}", package.name);
            let mut deb = package.metadata.take()
                .and_then(|m| m.deb).unwrap_or_default();
            let variant = deb.variants
                .as_mut()
                .and_then(|v| v.remove(variant))
                .ok_or_else(|| CargoDebError::VariantNotFound(variant.to_string()))?;
            variant.inherit_from(deb)
        } else {
            package.metadata.take().and_then(|m| m.deb).unwrap_or_default()
        };

        let separate_debug_symbols = separate_debug_symbols.unwrap_or_else(|| deb.separate_debug_symbols.unwrap_or(false));
        let compress_debug_symbols = compress_debug_symbols.unwrap_or_else(|| deb.compress_debug_symbols.unwrap_or(false));

        let debug_symbols = if separate_debug_symbols {
            if !debug_enabled {
                log::warn!("separate-debug-symbols implies strip");
            }
            DebugSymbols::Separate { compress: compress_debug_symbols }
        } else if debug_enabled {
            if compress_debug_symbols {
                log::warn!("separate-debug-symbols required to compress");
            }
            DebugSymbols::Keep
        } else {
            DebugSymbols::Strip
        };

        let (license_file, license_file_skip_lines) = manifest_license_file(package, deb.license_file.as_ref())?;

        manifest_check_config(package, &package_manifest_dir, &deb, listener);

        let extended_description_file = deb.extended_description_file.is_none()
            .then(|| package.readme().as_path()).flatten()
            .map(|readme_rel_path| package_manifest_dir.join(readme_rel_path));
        let extended_description = manifest_extended_description(
            deb.extended_description.take(),
            deb.extended_description_file.as_ref().map(Path::new).or(extended_description_file.as_deref()),
        )?;

        let mut config = Self {
            package_manifest_dir,
            deb_output_path,
            target: target.map(|t| t.to_string()),
            target_dir,
            deb: Package {
            default_timestamp,
            name: package.name.clone(),
            deb_name: deb.name.take().unwrap_or_else(|| debian_package_name(&package.name)),
            deb_version: deb_version.unwrap_or_else(|| manifest_version_string(package, deb_revision.or(deb.revision).as_deref()).into_owned()),
            license: package.license.take().map(|v| v.unwrap()),
            license_file,
            license_file_skip_lines,
            copyright: deb.copyright.take().ok_or_then(|| {
                if package.authors().is_empty() {
                    return Err("The package must have a copyright or authors property".into());
                }
                Ok(package.authors().join(", "))
            })?,
            homepage: package.homepage().map(From::from),
            documentation: package.documentation().map(From::from),
            repository: package.repository.take().map(|v| v.unwrap()),
            description: package.description.take().map(|v| v.unwrap()).unwrap_or_else(||format!("[generated from Rust crate {}]", package.name)),
            extended_description,
            maintainer: deb.maintainer.take().ok_or_then(|| {
                Ok(package.authors().first()
                    .ok_or("The package must have a maintainer or authors property")?.to_owned())
            })?,
            depends: deb.depends.take().map(DependencyList::into_depends_string).unwrap_or_else(|| "$auto".to_owned()),
            pre_depends: deb.pre_depends.take().map(DependencyList::into_depends_string),
            recommends: deb.recommends.take().map(DependencyList::into_depends_string),
            suggests: deb.suggests.take().map(DependencyList::into_depends_string),
            enhances: deb.enhances.take(),
            conflicts: deb.conflicts.take(),
            breaks: deb.breaks.take(),
            replaces: deb.replaces.take(),
            provides: deb.provides.take(),
            section: deb.section.take(),
            priority: deb.priority.take().unwrap_or_else(|| "optional".to_owned()),
            architecture: debian_architecture_from_rust_triple(target.unwrap_or(crate::DEFAULT_TARGET)).to_owned(),
            conf_files: deb.conf_files.map(|x| format_conffiles(&x)),
            assets: Assets::new(),
            triggers_file: deb.triggers_file.map(PathBuf::from),
            changelog: deb.changelog.take(),
            maintainer_scripts: deb.maintainer_scripts.map(PathBuf::from),
            preserve_symlinks: deb.preserve_symlinks.unwrap_or(false),
            systemd_units: match deb.systemd_units {
                None => None,
                Some(SystemUnitsSingleOrMultiple::Single(s)) => Some(vec![s]),
                Some(SystemUnitsSingleOrMultiple::Multi(v)) => Some(v),
            },
            },
            features: deb.features.take().unwrap_or_default(),
            default_features: deb.default_features.unwrap_or(true),
            debug_symbols,
        };
        config.take_assets(package, deb.assets.take(), &targets, selected_profile, listener)?;
        config.add_copyright_asset()?;
        config.add_changelog_asset()?;
        config.add_systemd_assets()?;

        Ok(config)
    }

    pub(crate) fn get_dependencies(&self, listener: &dyn Listener) -> CDResult<String> {
        let mut deps = HashSet::new();
        for word in self.deb.depends.split(',') {
            let word = word.trim();
            if word == "$auto" {
                let bin = self.deb.all_binaries();
                let resolved = bin.par_iter()
                    .filter(|bin| !bin.archive_as_symlink_only())
                    .filter_map(|p| p.path())
                    .filter_map(|bname| match resolve(bname, &self.target) {
                        Ok(bindeps) => Some(bindeps),
                        Err(err) => {
                            listener.warning(format!("{} (no auto deps for {})", err, bname.display()));
                            None
                        },
                    })
                    .collect::<Vec<_>>();
                for dep in resolved.into_iter().flat_map(|s| s.into_iter()) {
                    deps.insert(dep);
                }
            } else {
                let (dep, arch_spec) = get_architecture_specification(word)?;
                if let Some(spec) = arch_spec {
                    if match_architecture(spec, &self.deb.architecture)? {
                        deps.insert(dep);
                    }
                } else {
                    deps.insert(dep);
                }
            }
        }
        Ok(deps.into_iter().collect::<Vec<_>>().join(", "))
    }

    pub fn extend_cargo_build_flags(&self, flags: &mut Vec<String>) {
        if flags.iter().any(|f| f == "--workspace" || f == "--all") {
            return;
        }

        for a in self.deb.assets.unresolved.iter().filter(|a| a.c.is_built != IsBuilt::No) {
            if is_glob_pattern(&a.source_path) {
                log::debug!("building entire workspace because of glob {}", a.source_path.display());
                flags.push("--workspace".into());
                return;
            }
        }

        let mut build_bins = vec![];
        let mut build_examples = vec![];
        let mut build_libs = false;
        let mut same_package = true;
        let resolved = self.deb.assets.resolved.iter().map(|a| (&a.c, a.source.path()));
        let unresolved = self.deb.assets.unresolved.iter().map(|a| (&a.c, Some(a.source_path.as_ref())));
        for (asset_target, source_path) in resolved.chain(unresolved).filter(|(c,_)| c.is_built != IsBuilt::No) {
            if asset_target.is_built != IsBuilt::SamePackage {
                log::debug!("building workspace because {} is from another package", source_path.unwrap_or(&asset_target.target_path).display());
                same_package = false;
            }
            if asset_target.is_dynamic_library() || source_path.map_or(false, is_dynamic_library_filename) {
                log::debug!("building libs for {}", source_path.unwrap_or(&asset_target.target_path).display());
                build_libs = true;
            } else if asset_target.is_executable() {
                if let Some(source_path) = source_path {
                    let name = source_path.file_name().unwrap().to_str().expect("utf-8 target name");
                    let name = name.strip_suffix(EXE_SUFFIX).unwrap_or(name);
                    if asset_target.is_example {
                        build_examples.push(name);
                    } else {
                        build_bins.push(name);
                    }
                }
            }
        }

        if !same_package {
            flags.push("--workspace".into());
        }
        flags.extend(build_bins.iter().map(|name| {
            log::debug!("building bin for {}", name);
            format!("--bin={name}")
        }));
        flags.extend(build_examples.iter().map(|name| {
            log::debug!("building example for {}", name);
            format!("--example={name}")
        }));
        if build_libs {
            flags.push("--lib".into());
        }
    }

    pub fn resolve_assets(&mut self) -> CDResult<()> {
        for UnresolvedAsset { source_path, c: AssetCommon { target_path, chmod, is_built, is_example } } in self.deb.assets.unresolved.drain(..) {
            let source_prefix: PathBuf = source_path.iter()
                .take_while(|part| !is_glob_pattern(part.as_ref()))
                .collect();
            let source_is_glob = is_glob_pattern(&source_path);
            let file_matches = glob::glob(source_path.to_str().expect("utf8 path"))?
                // Remove dirs from globs without throwing away errors
                .map(|entry| {
                    let source_file = entry?;
                    Ok(if source_file.is_dir() { None } else { Some(source_file) })
                })
                .filter_map(|res| match res {
                    Ok(None) => None,
                    Ok(Some(x)) => Some(Ok(x)),
                    Err(x) => Some(Err(x)),
                })
                .collect::<CDResult<Vec<_>>>()?;

            // If glob didn't match anything, it's likely an error
            // as all files should exist when called to resolve
            if file_matches.is_empty() {
                return Err(CargoDebError::AssetFileNotFound(source_path));
            }

            for source_file in file_matches {
                // XXX: how do we handle duplicated assets?
                let target_file = if source_is_glob {
                    target_path.join(source_file.strip_prefix(&source_prefix).unwrap())
                } else {
                    target_path.clone()
                };
                log::debug!("asset {} -> {} {} {:o}", source_file.display(), target_file.display(), if is_built == IsBuilt::No {"copy"} else {"build"}, chmod);
                self.deb.assets.resolved.push(Asset::new(
                    AssetSource::from_path(source_file, self.deb.preserve_symlinks),
                    target_file,
                    chmod,
                    is_built,
                    is_example,
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn add_copyright_asset(&mut self) -> CDResult<()> {
        let (source_path, copyright_file) = self.generate_copyright_asset()?;
        log::debug!("added copyright via {}", source_path.display());
        self.deb.assets.resolved.push(Asset::new(
            AssetSource::Data(copyright_file),
            Path::new("usr/share/doc").join(&self.deb.deb_name).join("copyright"),
            0o644,
            IsBuilt::No,
            false,
        ).processed("generated", source_path));
        Ok(())
    }

    /// Generates the copyright file from the license file and adds that to the tar archive.
    fn generate_copyright_asset(&self) -> CDResult<(PathBuf, Vec<u8>)> {
        let mut copyright: Vec<u8> = Vec::new();
        let source_path;
        if let Some(ref path) = self.deb.license_file {
            source_path = self.path_in_package(path);
            let license_string = fs::read_to_string(&source_path)
                .map_err(|e| CargoDebError::IoFile("unable to read license file", e, path.clone()))?;
            if !has_copyright_metadata(&license_string) {
                self.deb.append_copyright_metadata(&mut copyright)?;
            }

            // Skip the first `A` number of lines and then iterate each line after that.
            for line in license_string.lines().skip(self.deb.license_file_skip_lines) {
                // If the line is a space, add a dot, else write the line.
                if line == " " {
                    copyright.write_all(b" .\n")?;
                } else {
                    copyright.write_all(line.as_bytes())?;
                    copyright.write_all(b"\n")?;
                }
            }
        } else {
            source_path = "Cargo.toml".into();
            self.deb.append_copyright_metadata(&mut copyright)?;
        }

        Ok((source_path, copyright))
    }

    fn add_changelog_asset(&mut self) -> CDResult<()> {
        // The file is autogenerated later
        if self.deb.changelog.is_some() {
            if let Some((source_path, changelog_file)) = self.generate_changelog_asset()? {
                log::debug!("added changelog via {}", source_path.display());
                self.deb.assets.resolved.push(Asset::new(
                    AssetSource::Data(changelog_file),
                    Path::new("usr/share/doc").join(&self.deb.deb_name).join("changelog.Debian.gz"),
                    0o644,
                    IsBuilt::No,
                    false,
                ).processed("generated", source_path));
            }
        }
        Ok(())
    }

    /// Generates compressed changelog file
    pub(crate) fn generate_changelog_asset(&self) -> CDResult<Option<(PathBuf, Vec<u8>)>> {
        if let Some(ref path) = self.deb.changelog {
            let source_path = self.path_in_package(path);
            let changelog = fs::read(&source_path)
                .and_then(|content| {
                    // allow pre-compressed
                    if source_path.extension().is_some_and(|e| e == "gz") {
                        return Ok(content);
                    }
                    // The input is plaintext, but the debian package should contain gzipped one.
                    compress::gzipped(&content)
                })
                .map_err(|e| CargoDebError::IoFile("unable to read changelog file", e, source_path.clone()))?;
            Ok(Some((source_path, changelog)))
        } else {
            Ok(None)
        }
    }

    fn add_systemd_assets(&mut self) -> CDResult<()> {
        if let Some(ref config_vec) = self.deb.systemd_units {
            for config in config_vec {
                let units_dir_option = config.unit_scripts.as_ref()
                    .or(self.deb.maintainer_scripts.as_ref());
                if let Some(unit_dir) = units_dir_option {
                    let search_path = self.path_in_package(unit_dir);
                    let package = &self.deb.name;
                    let unit_name = config.unit_name.as_deref();

                    let units = dh_installsystemd::find_units(&search_path, package, unit_name);

                    for (source, target) in units {
                        self.deb.assets.resolved.push(Asset::new(
                            AssetSource::from_path(source, self.deb.preserve_symlinks), // should this even support symlinks at all?
                            target.path,
                            target.mode,
                            IsBuilt::No,
                            false,
                        ));
                    }
                }
            }
        } else {
            log::debug!("no systemd units to generate");
        }
        Ok(())
    }

    pub(crate) fn path_in_build<P: AsRef<Path>>(&self, rel_path: P, profile: &str) -> PathBuf {
        self.path_in_build_(rel_path.as_ref(), profile)
    }

    pub(crate) fn path_in_build_(&self, rel_path: &Path, profile: &str) -> PathBuf {
        let profile = match profile {
            "dev" => "debug",
            p => p,
        };

        let mut path = self.target_dir.join(profile);
        path.push(rel_path);
        path
    }

    pub(crate) fn path_in_package<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.package_manifest_dir.join(rel_path)
    }

    /// Store intermediate files here
    pub(crate) fn deb_temp_dir(&self) -> PathBuf {
        self.target_dir.join("debian").join(&self.deb.name)
    }

    /// Save final .deb here
    pub(crate) fn deb_output_path(&self) -> PathBuf {
        let filename = format!("{}_{}_{}.deb", self.deb.deb_name, self.deb.deb_version, self.deb.architecture);

        if let Some(ref path_str) = self.deb_output_path {
            let path = Path::new(path_str);
            if path_str.ends_with('/') || path.is_dir() {
                path.join(filename)
            } else {
                path.to_owned()
            }
        } else {
            self.default_deb_output_dir().join(filename)
        }
    }

    pub(crate) fn default_deb_output_dir(&self) -> PathBuf {
        self.target_dir.join("debian")
    }

    pub(crate) fn cargo_config(&self) -> CDResult<Option<CargoConfig>> {
        CargoConfig::new(&self.package_manifest_dir)
    }
}

impl Package {
    pub(crate) fn filename_glob(&self) -> String {
        format!("{}_*_{}.deb", self.deb_name, self.architecture)
    }

    /// Executables AND dynamic libraries. May include symlinks.
    fn all_binaries(&self) -> Vec<&AssetSource> {
        self.assets.resolved.iter()
            .filter(|asset| {
                // Assumes files in build dir which have executable flag set are binaries
                asset.c.is_dynamic_library() || asset.c.is_executable()
            })
            .map(|asset| &asset.source)
            .collect()
    }

    /// Executables AND dynamic libraries, but only in `target/release`
    pub(crate) fn built_binaries_mut(&mut self) -> Vec<&mut Asset> {
        self.assets.resolved.iter_mut()
            .filter(move |asset| {
                // Assumes files in build dir which have executable flag set are binaries
                asset.c.is_built != IsBuilt::No && (asset.c.is_dynamic_library() || asset.c.is_executable())
            })
            .collect()
    }


    /// similar files next to each other improve tarball compression
    pub fn sort_assets_by_type(&mut self) {
        self.assets.resolved.sort_by(|a,b| {
            a.c.is_executable().cmp(&b.c.is_executable())
            .then(a.c.is_dynamic_library().cmp(&b.c.is_dynamic_library()))
            .then(a.c.target_path.extension().cmp(&b.c.target_path.extension()))
            .then(a.c.target_path.cmp(&b.c.target_path))
        });
    }

    /// Creates the sha256sums file which contains a list of all contained files and the sha256sums of each.
    pub fn generate_sha256sums(&self, asset_hashes: &HashMap<PathBuf, [u8; 32]>) -> CDResult<Vec<u8>> {
        let mut sha256sums: Vec<u8> = Vec::with_capacity(self.assets.resolved.len() * 80);

        // Collect sha256sums from each asset in the archive (excludes symlinks).
        for asset in &self.assets.resolved {
            if let Some(value) = asset_hashes.get(&asset.c.target_path) {
                for &b in value {
                    write!(sha256sums, "{b:02x}")?;
                }
                sha256sums.write_all(b"  ")?;

                sha256sums.write_all(&asset.c.target_path.as_path().as_unix_path())?;
                sha256sums.write_all(b"\n")?;
            }
        }
        Ok(sha256sums)
    }

    /// Generates the control file that obtains all the important information about the package.
    pub fn generate_control(&self, deps: &str) -> CDResult<Vec<u8>> {
        // Create and return the handle to the control file with write access.
        let mut control: Vec<u8> = Vec::with_capacity(1024);

        // Write all of the lines required by the control file.
        writeln!(&mut control, "Package: {}", self.deb_name)?;
        writeln!(&mut control, "Version: {}", self.deb_version)?;
        writeln!(&mut control, "Architecture: {}", self.architecture)?;
        if let Some(ref repo) = self.repository {
            if repo.starts_with("http") {
                writeln!(&mut control, "Vcs-Browser: {repo}")?;
            }
            if let Some(kind) = self.repository_type() {
                writeln!(&mut control, "Vcs-{kind}: {repo}")?;
            }
        }
        if let Some(homepage) = self.homepage.as_ref().or(self.documentation.as_ref()) {
            writeln!(&mut control, "Homepage: {homepage}")?;
        }
        if let Some(ref section) = self.section {
            writeln!(&mut control, "Section: {section}")?;
        }
        writeln!(&mut control, "Priority: {}", self.priority)?;
        writeln!(&mut control, "Maintainer: {}", self.maintainer)?;

        let installed_size = self.assets.resolved
            .iter()
            .map(|m| (m.source.file_size().unwrap_or(0) + 2047) / 1024) // assume 1KB of fs overhead per file
            .sum::<u64>();

        writeln!(&mut control, "Installed-Size: {installed_size}")?;

        if !deps.is_empty() {
            writeln!(&mut control, "Depends: {deps}")?;
        }

        if let Some(ref pre_depends) = self.pre_depends {
            let pre_depends_normalized = pre_depends.trim();

            if !pre_depends_normalized.is_empty() {
                writeln!(&mut control, "Pre-Depends: {pre_depends_normalized}")?;
            }
        }

        if let Some(ref recommends) = self.recommends {
            let recommends_normalized = recommends.trim();

            if !recommends_normalized.is_empty() {
                writeln!(&mut control, "Recommends: {recommends_normalized}")?;
            }
        }

        if let Some(ref suggests) = self.suggests {
            let suggests_normalized = suggests.trim();

            if !suggests_normalized.is_empty() {
                writeln!(&mut control, "Suggests: {suggests_normalized}")?;
            }
        }

        if let Some(ref enhances) = self.enhances {
            let enhances_normalized = enhances.trim();

            if !enhances_normalized.is_empty() {
                writeln!(&mut control, "Enhances: {enhances_normalized}")?;
            }
        }

        if let Some(ref conflicts) = self.conflicts {
            writeln!(&mut control, "Conflicts: {conflicts}")?;
        }
        if let Some(ref breaks) = self.breaks {
            writeln!(&mut control, "Breaks: {breaks}")?;
        }
        if let Some(ref replaces) = self.replaces {
            writeln!(&mut control, "Replaces: {replaces}")?;
        }
        if let Some(ref provides) = self.provides {
            writeln!(&mut control, "Provides: {provides}")?;
        }

        write!(&mut control, "Description:")?;
        for line in self.description.split_by_chars(79) {
            writeln!(&mut control, " {line}")?;
        }

        if let Some(ref desc) = self.extended_description {
            for line in desc.split_by_chars(79) {
                writeln!(&mut control, " {line}")?;
            }
        }
        control.push(10);

        Ok(control)
    }

    /// Tries to guess type of source control used for the repo URL.
    /// It's a guess, and it won't be 100% accurate, because Cargo suggests using
    /// user-friendly URLs or webpages instead of tool-specific URL schemes.
    pub(crate) fn repository_type(&self) -> Option<&str> {
        if let Some(ref repo) = self.repository {
            if repo.starts_with("git+") ||
                repo.ends_with(".git") ||
                repo.contains("git@") ||
                repo.contains("github.com") ||
                repo.contains("gitlab.com")
            {
                return Some("Git");
            }
            if repo.starts_with("cvs+") || repo.contains("pserver:") || repo.contains("@cvs.") {
                return Some("Cvs");
            }
            if repo.starts_with("hg+") || repo.contains("hg@") || repo.contains("/hg.") {
                return Some("Hg");
            }
            if repo.starts_with("svn+") || repo.contains("/svn.") {
                return Some("Svn");
            }
            return None;
        }
        None
    }

    pub(crate) fn append_copyright_metadata(&self, copyright: &mut Vec<u8>) -> Result<(), CargoDebError> {
        writeln!(copyright, "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/")?;
        writeln!(copyright, "Upstream-Name: {}", self.name)?;
        if let Some(source) = self.repository.as_ref().or(self.homepage.as_ref()) {
            writeln!(copyright, "Source: {source}")?;
        }
        writeln!(copyright, "Copyright: {}", self.copyright)?;
        if let Some(ref license) = self.license {
            writeln!(copyright, "License: {license}")?;
        }
        Ok(())
    }
}

fn has_copyright_metadata(file: &str) -> bool {
    file.lines().take(10)
        .any(|l| l.starts_with("License: ") || l.starts_with("Source: ") || l.starts_with("Upstream-Name: ") || l.starts_with("Format: "))
}

/// Debian doesn't like `_` in names
fn debian_package_name(crate_name: &str) -> String {
    // crate names are ASCII only
    crate_name.bytes().map(|c| {
        if c != b'_' {c.to_ascii_lowercase() as char} else {'-'}
    }).collect()
}

fn manifest_check_config(package: &cargo_toml::Package<CargoPackageMetadata>, manifest_dir: &Path, deb: &CargoDeb, listener: &dyn Listener) {
    let readme_rel_path = package.readme().as_path();
    if package.description().is_none() {
        listener.warning("description field is missing in Cargo.toml".to_owned());
    }
    if package.license().is_none() && package.license_file().is_none() {
        listener.warning("license field is missing in Cargo.toml".to_owned());
    }
    if let Some(readme_rel_path) = readme_rel_path {
        let ext = readme_rel_path.extension().unwrap_or("".as_ref());
        if deb.extended_description.is_none() && deb.extended_description_file.is_none() && (ext == "md" || ext == "markdown") {
            listener.info(format!("extended-description field missing. Using {}, but markdown may not render well.", readme_rel_path.display()));
        }
    } else {
        for p in &["README.md", "README.markdown", "README.txt", "README"] {
            if manifest_dir.join(p).exists() {
                listener.warning(format!("{p} file exists, but is not specified in `readme` Cargo.toml field"));
                break;
            }
        }
    }
}

fn manifest_extended_description(desc: Option<String>, desc_file: Option<&Path>) -> CDResult<Option<String>> {
    Ok(if desc.is_some() {
        desc
    } else if let Some(desc_file) = desc_file {
        Some(fs::read_to_string(desc_file)
            .map_err(|err| CargoDebError::IoFile(
                    "unable to read extended description from file", err, PathBuf::from(desc_file)))?)
    } else {
        None
    })
}

impl Config {
    fn take_assets(&mut self, package: &cargo_toml::Package<CargoPackageMetadata>, assets: Option<Vec<Vec<String>>>, build_targets: &[CargoMetadataTarget], profile: &str, listener: &dyn Listener) -> CDResult<()> {
        let assets = if let Some(assets) = assets {
            let profile_target_dir = format!("target/{profile}");
            // Treat all explicit assets as unresolved until after the build step
            let mut unresolved_assets = Vec::with_capacity(assets.len());
            for mut asset_line in assets {
                let mut asset_parts = asset_line.drain(..);
                let source_path = PathBuf::from(asset_parts.next()
                    .ok_or("missing path (first array entry) for asset in Cargo.toml")?);
                if source_path.starts_with("target/debug/") {
                    listener.warning(format!("Packaging of development-only binaries is intentionally unsupported in cargo-deb.
    Please only use `target/release/` directory for built products, not `{}`.
    To add debug information or additional assertions use `[profile.release]` in `Cargo.toml` instead.
    This will be hard error in a future release of cargo-deb.", source_path.display()));
                }
                // target/release is treated as a magic prefix that resolves to any profile
                let (is_built, source_path, is_example) = if let Ok(rel_path) = source_path.strip_prefix("target/release").or_else(|_| source_path.strip_prefix(&profile_target_dir)) {
                    let is_example = rel_path.starts_with("examples");

                    (self.find_is_built_file_in_package(rel_path, build_targets, if is_example { "example" } else { "bin" }), self.path_in_build(rel_path, profile), is_example)
                } else {
                    (IsBuilt::No, self.path_in_package(&source_path), false)
                };
                let target_path = PathBuf::from(asset_parts.next().ok_or("missing target (second array entry) for asset in Cargo.toml. Use something like \"usr/local/bin/\".")?);
                let chmod = u32::from_str_radix(&asset_parts.next().ok_or("missing chmod (third array entry) for asset in Cargo.toml. Use an octal string like \"777\".")?, 8)
                    .map_err(|e| CargoDebError::NumParse("unable to parse chmod argument", e))?;

                unresolved_assets.push(UnresolvedAsset {
                    source_path,
                    c: AssetCommon { target_path, chmod, is_built, is_example },
                });
            }
            Assets::with_unresolved_assets(unresolved_assets)
        } else {
            let mut implied_assets: Vec<_> = build_targets.iter()
                .filter_map(|t| {
                    if t.crate_types.iter().any(|ty| ty == "bin") && t.kind.iter().any(|k| k == "bin") {
                        Some(Asset::new(
                            AssetSource::Path(self.path_in_build(&t.name, profile)),
                            Path::new("usr/bin").join(&t.name),
                            0o755,
                            self.is_built_file_in_package(t),
                            false,
                        ))
                    } else if t.crate_types.iter().any(|ty| ty == "cdylib") && t.kind.iter().any(|k| k == "cdylib") {
                        // FIXME: std has constants for the host arch, but not for cross-compilation
                        let lib_name = format!("{DLL_PREFIX}{}{DLL_SUFFIX}", t.name);
                        Some(Asset::new(
                            AssetSource::Path(self.path_in_build(&lib_name, profile)),
                            Path::new("usr/lib").join(lib_name),
                            0o644,
                            self.is_built_file_in_package(t),
                            false,
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            if let Some(readme_rel_path) = package.readme().as_path() {
                let path = self.path_in_package(readme_rel_path);
                let target_path = Path::new("usr/share/doc")
                    .join(&package.name)
                    .join(path.file_name().ok_or("bad README path")?);
                implied_assets.push(Asset::new(AssetSource::Path(path), target_path, 0o644, IsBuilt::No, false));
            }
            Assets::with_resolved_assets(implied_assets)
        };
        if assets.is_empty() {
            return Err("No binaries or cdylibs found. The package is empty. Please specify some assets to package in Cargo.toml".into());
        }
        self.deb.assets = assets;
        Ok(())
    }

    fn find_is_built_file_in_package(&self, rel_path: &Path, build_targets: &[CargoMetadataTarget], expected_kind: &str) -> IsBuilt {
        let source_name = rel_path.file_name().expect("asset filename").to_str().expect("utf-8 names");
        let source_name = source_name.strip_suffix(EXE_SUFFIX).unwrap_or(source_name);

        if build_targets.iter()
            .filter(|t| t.name == source_name && t.kind.iter().any(|k| k == expected_kind))
            .any(|t| self.is_built_file_in_package(t) == IsBuilt::SamePackage)
        {
            IsBuilt::SamePackage
        } else {
            IsBuilt::Workspace
        }
    }

    fn is_built_file_in_package(&self, build_target: &CargoMetadataTarget) -> IsBuilt {
        if build_target.src_path.starts_with(&self.package_manifest_dir) {
            IsBuilt::SamePackage
        } else {
            IsBuilt::Workspace
        }
    }
}

/// Format conffiles section, ensuring each path has a leading slash
///
/// Starting with [dpkg 1.20.1](https://github.com/guillemj/dpkg/blob/68ab722604217d3ab836276acfc0ae1260b28f5f/debian/changelog#L393),
/// which is what Ubuntu 21.04 uses, relative conf-files are no longer
/// accepted (the deb-conffiles man page states that "they should be listed as
/// absolute pathnames"). So we prepend a leading slash to the given strings
/// as needed
fn format_conffiles<S: AsRef<str>>(files: &[S]) -> String {
    files.iter().fold(String::new(), |mut acc, x| {
        let pth = x.as_ref();
        if !pth.starts_with('/') {
            acc.push('/');
        }
        acc + pth + "\n"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::manifest::SystemdUnitsConfig;
    use crate::util::tests::add_test_fs_paths;

    #[test]
    fn match_arm_arch() {
        assert_eq!("armhf", debian_architecture_from_rust_triple("arm-unknown-linux-gnueabihf"));
    }

    #[test]
    fn arch_spec() {
        use ArchSpec::*;
        // req
        assert_eq!(
            get_architecture_specification("libjpeg64-turbo [armhf]").expect("arch"),
            ("libjpeg64-turbo".to_owned(), Some(Require("armhf".to_owned()))));
        // neg
        assert_eq!(
            get_architecture_specification("libjpeg64-turbo [!amd64]").expect("arch"),
            ("libjpeg64-turbo".to_owned(), Some(NegRequire("amd64".to_owned()))));
    }

    #[test]
    fn assets() {
        let a = Asset::new(
            AssetSource::Path(PathBuf::from("target/release/bar")),
            PathBuf::from("baz/"),
            0o644,
            IsBuilt::SamePackage,
            false,
        );
        assert_eq!("baz/bar", a.c.target_path.to_str().unwrap());
        assert!(a.c.is_built != IsBuilt::No);

        let a = Asset::new(
            AssetSource::Path(PathBuf::from("foo/bar")),
            PathBuf::from("/baz/quz"),
            0o644,
            IsBuilt::No,
            false,
        );
        assert_eq!("baz/quz", a.c.target_path.to_str().unwrap());
        assert!(a.c.is_built == IsBuilt::No);
    }

    /// Tests that getting the debug filename from a path returns the same path
    /// with ".debug" appended
    #[test]
    fn test_debug_filename() {
        let path = Path::new("/my/test/file");
        assert_eq!(debug_filename(path), Path::new("/my/test/file.debug"));
    }

    /// Tests that getting the debug target for an Asset that `is_built` returns
    /// the path "/usr/lib/debug/<path-to-target>.debug"
    #[test]
    fn test_debug_target_ok() {
        let a = Asset::new(
            AssetSource::Path(PathBuf::from("target/release/bar")),
            PathBuf::from("/usr/bin/baz/"),
            0o644,
            IsBuilt::SamePackage,
            false,
        );
        let debug_target = a.c.default_debug_target_path();
        assert_eq!(debug_target, Path::new("/usr/lib/debug/usr/bin/baz/bar.debug"));
    }

    /// Tests that getting the debug target for an Asset that `is_built` and that
    /// has a relative path target returns the path "/usr/lib/debug/<path-to-target>.debug"
    #[test]
    fn test_debug_target_ok_relative() {
        let a = Asset::new(
            AssetSource::Path(PathBuf::from("target/release/bar")),
            PathBuf::from("baz/"),
            0o644,
            IsBuilt::Workspace,
            false,
        );
        let debug_target = a.c.default_debug_target_path();
        assert_eq!(debug_target, Path::new("/usr/lib/debug/baz/bar.debug"));
    }

    fn to_canon_static_str(s: &str) -> &'static str {
        let cwd = std::env::current_dir().unwrap();
        let abs_path = cwd.join(s);
        let abs_path_string = abs_path.to_string_lossy().into_owned();
        Box::leak(abs_path_string.into_boxed_str())
    }

    #[test]
    fn add_systemd_assets_with_no_config_does_nothing() {
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_info().return_const(());

        // supply a systemd unit file as if it were available on disk
        let _g = add_test_fs_paths(&[to_canon_static_str("cargo-deb.service")]);

        let config = Config::from_manifest(Some(Path::new("Cargo.toml")), None, None, None, None, None, None, &mock_listener, "release", None, None).unwrap();

        let num_unit_assets = config.deb.assets.resolved.iter()
            .filter(|a| a.c.target_path.starts_with("lib/systemd/system/"))
            .count();

        assert_eq!(0, num_unit_assets);
    }

    #[test]
    fn add_systemd_assets_with_config_adds_unit_assets() {
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_info().return_const(());

        // supply a systemd unit file as if it were available on disk
        let _g = add_test_fs_paths(&[to_canon_static_str("cargo-deb.service")]);

        let mut config = Config::from_manifest(Some(Path::new("Cargo.toml")), None, None, None, None, None, None, &mock_listener, "release", None, None).unwrap();

        config.deb.systemd_units.get_or_insert(vec![SystemdUnitsConfig::default()]);
        config.deb.maintainer_scripts.get_or_insert(PathBuf::new());

        config.add_systemd_assets().unwrap();

        let num_unit_assets = config.deb.assets.resolved
            .iter()
            .filter(|a| a.c.target_path.starts_with("lib/systemd/system/"))
            .count();

        assert_eq!(1, num_unit_assets);
    }

    #[test]
    fn format_conffiles_empty() {
        let actual = format_conffiles::<String>(&[]);
        assert_eq!("", actual);
    }

    #[test]
    fn format_conffiles_one() {
        let actual = format_conffiles(&["/etc/my-pkg/conf.toml"]);
        assert_eq!("/etc/my-pkg/conf.toml\n", actual);
    }

    #[test]
    fn format_conffiles_multiple() {
        let actual = format_conffiles(&["/etc/my-pkg/conf.toml", "etc/my-pkg/conf2.toml"]);

        assert_eq!("/etc/my-pkg/conf.toml\n/etc/my-pkg/conf2.toml\n", actual);
    }
}
