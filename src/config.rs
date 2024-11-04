use crate::assets::is_dynamic_library_filename;
use crate::assets::{Asset, AssetSource, Assets, IsBuilt, UnresolvedAsset, RawAsset};
use crate::util::compress::gzipped;
use crate::{debian_architecture_from_rust_triple, CargoLockingFlags};
use crate::dependencies::resolve;
use crate::dh::dh_installsystemd;
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::parse::cargo::CargoConfig;
use crate::parse::manifest::{cargo_metadata, manifest_debug_flag, manifest_version_string, LicenseFile};
use crate::parse::manifest::{CargoDeb, CargoDebAssetArrayOrTable, CargoMetadataTarget, CargoPackageMetadata, ManifestFound};
use crate::parse::manifest::{DependencyList, SystemUnitsSingleOrMultiple, SystemdUnitsConfig};
use crate::util::ok_or::OkOrThen;
use crate::util::pathbytes::AsUnixPathBytes;
use crate::util::wordsplit::WordSplit;
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env::consts::{DLL_PREFIX, DLL_SUFFIX, EXE_SUFFIX};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use std::{fs, io};

pub(crate) fn is_glob_pattern(s: &Path) -> bool {
    s.to_bytes().iter().any(|&c| c == b'*' || c == b'[' || c == b']' || c == b'!')
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

    /// "release" if None
    build_profile_override: Option<String>,

    /// Products available in the package
    build_targets: Vec<CargoMetadataTarget>,
    cargo_locking_flags: CargoLockingFlags,
}

#[derive(Debug)]
pub enum ExtendedDescription {
    None,
    File(PathBuf),
    String(String),
    ReadmeFallback(PathBuf),
}

#[derive(Debug)]
#[non_exhaustive]
pub struct PackageConfig {
    /// The name of the project to build
    pub name: String,
    /// The name to give the Debian package; usually the same as the Cargo project name
    pub deb_name: String,
    /// The version to give the Debian package; usually the same as the Cargo version
    pub deb_version: String,
    /// The software license of the project (SPDX format).
    pub license: Option<String>,
    /// The location of the license file
    pub license_file_rel_path: Option<PathBuf>,
    /// number of lines to skip when reading `license_file`
    pub license_file_skip_lines: usize,
    /// The copyright of the project
    /// (Debian's `copyright` file contents).
    pub copyright: Option<String>,
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
    pub extended_description: ExtendedDescription,
    /// The maintainer of the Debian package.
    /// In Debian `control` file `Maintainer` field format.
    pub maintainer: String,
    /// Deps including `$auto`
    pub wildcard_depends: String,
    /// The Debian dependencies required to run the project.
    pub resolved_depends: Option<String>,
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
    /// Automatically includes all files in `/etc`
    pub conf_files: Vec<String>,
    /// All of the files that are to be packaged.
    pub(crate) assets: Assets,
    pub(crate) raw_assets: Option<Vec<RawAsset>>,

    /// Added to usr/share/doc as a fallback
    pub readme_rel_path: Option<PathBuf>,
    /// The location of the triggers file
    pub triggers_file_rel_path: Option<PathBuf>,
    /// The path where possible maintainer scripts live
    pub maintainer_scripts_rel_path: Option<PathBuf>,
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

/// Replace config values via command-line
#[derive(Debug, Clone, Default)]
pub struct DebConfigOverrides {
    pub deb_version: Option<String>,
    pub deb_revision: Option<String>,
    pub maintainer: Option<String>,
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
        overrides: DebConfigOverrides,
        build_profile_override: Option<String>,
        separate_debug_symbols: Option<bool>,
        compress_debug_symbols: Option<bool>,
        cargo_locking_flags: CargoLockingFlags,
        listener: &dyn Listener,
    ) -> CDResult<(Self, PackageConfig)> {
        // **IMPORTANT**: This function must not create or expect to see any asset files on disk!
        // It's run before destination directory is cleaned up, and before the build start!

        let ManifestFound {
            build_targets,
            root_manifest,
            mut manifest_path,
            mut target_dir,
            mut manifest,
        } = cargo_metadata(root_manifest_path, selected_package_name, cargo_locking_flags)?;

        let default_timestamp = if let Ok(source_date_epoch) = std::env::var("SOURCE_DATE_EPOCH") {
            source_date_epoch.parse().map_err(|e| CargoDebError::NumParse("SOURCE_DATE_EPOCH", e))?
        } else {
            let manifest_mdate = fs::metadata(&manifest_path)?.modified().unwrap_or_else(|_| SystemTime::now());
            let mut timestamp = manifest_mdate.duration_since(SystemTime::UNIX_EPOCH).map_err(CargoDebError::SystemTime)?.as_secs();
            timestamp -= timestamp % (24 * 3600);
            timestamp
        };

        manifest_path.pop();
        let manifest_dir = manifest_path;

        // Cargo cross-compiles to a dir
        if let Some(target) = target {
            target_dir.push(target);
        };

        let selected_profile = build_profile_override.as_deref().unwrap_or("release");

        let debug_enabled = manifest_debug_flag(&manifest, selected_profile)
            .or_else(move || manifest_debug_flag(root_manifest.as_ref()?, selected_profile))
            .unwrap_or(false);

        let cargo_package = manifest.package.as_mut().ok_or("bad package")?;

        // If we build against a variant use that config and change the package name
        let mut deb = if let Some(variant) = variant {
            // Use dash as underscore is not allowed in package names
            cargo_package.name = format!("{}-{variant}", cargo_package.name);
            let mut deb = cargo_package.metadata.take()
                .and_then(|m| m.deb).unwrap_or_default();
            let variant = deb.variants
                .as_mut()
                .and_then(|v| v.remove(variant))
                .ok_or_else(|| CargoDebError::VariantNotFound(variant.to_string()))?;
            variant.inherit_from(deb)
        } else {
            cargo_package.metadata.take().and_then(|m| m.deb).unwrap_or_default()
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

        let config = Self {
            package_manifest_dir: manifest_dir,
            deb_output_path,
            target: target.map(|t| t.to_string()),
            target_dir,
            features: deb.features.take().unwrap_or_default(),
            default_features: deb.default_features.unwrap_or(true),
            debug_symbols,
            build_profile_override,
            build_targets,
            cargo_locking_flags,
        };

        let package_deb = PackageConfig::new(deb, cargo_package, listener, default_timestamp, overrides, target)?;

        Ok((config, package_deb))
    }

    pub fn prepare_assets_before_build(&self, package_deb: &mut PackageConfig) -> CDResult<()> {
        package_deb.assets = if let Some(raw_assets) = package_deb.raw_assets.take() {
            self.explicit_assets(raw_assets)?
        } else {
            self.implicit_assets(&package_deb.deb_name, package_deb.readme_rel_path.as_deref())?
        };
        self.add_copyright_asset(package_deb)?;
        self.add_changelog_asset(package_deb)?;
        self.add_systemd_assets(package_deb)?;

        self.reset_deb_temp_directory(package_deb)?;
        Ok(())
    }

    pub fn set_cargo_build_flags_for_package(&self, package_deb: &PackageConfig, flags: &mut Vec<String>) {
        flags.push(self.build_profile_override.as_deref().map(|p| format!("--profile={p}")).unwrap_or("--release".into()));
        flags.extend(self.cargo_locking_flags.flags().map(String::from));

        if flags.iter().any(|f| f == "--workspace" || f == "--all") {
            return;
        }

        for a in package_deb.assets.unresolved.iter().filter(|a| a.c.is_built()) {
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
        let resolved = package_deb.assets.resolved.iter().map(|a| (&a.c, a.source.path()));
        let unresolved = package_deb.assets.unresolved.iter().map(|a| (&a.c, Some(a.source_path.as_ref())));
        for (asset_target, source_path) in resolved.chain(unresolved).filter(|(c, _)| c.is_built()) {
            if !asset_target.is_same_package() {
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

    fn add_copyright_asset(&self, package_deb: &mut PackageConfig) -> CDResult<()> {
        let (source_path, copyright_file) = self.generate_copyright_asset(package_deb)?;
        log::debug!("added copyright via {}", source_path.display());
        package_deb.assets.resolved.push(Asset::new(
            AssetSource::Data(copyright_file),
            Path::new("usr/share/doc").join(&package_deb.deb_name).join("copyright"),
            0o644,
            IsBuilt::No,
            false,
        ).processed("generated", source_path));
        Ok(())
    }

    /// Generates the copyright file from the license file and adds that to the tar archive.
    fn generate_copyright_asset(&self, package_deb: &PackageConfig) -> CDResult<(PathBuf, Vec<u8>)> {
        let mut copyright: Vec<u8> = Vec::new();
        let source_path;
        if let Some(path) = &package_deb.license_file_rel_path {
            source_path = self.path_in_package(path);
            let license_string = fs::read_to_string(&source_path)
                .map_err(|e| CargoDebError::IoFile("unable to read license file", e, path.clone()))?;
            if !has_copyright_metadata(&license_string) {
                package_deb.append_copyright_metadata(&mut copyright)?;
            }

            // Skip the first `A` number of lines and then iterate each line after that.
            for line in license_string.lines().skip(package_deb.license_file_skip_lines) {
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
            package_deb.append_copyright_metadata(&mut copyright)?;
        }

        Ok((source_path, copyright))
    }

    fn add_changelog_asset(&self, package_deb: &mut PackageConfig) -> CDResult<()> {
        if package_deb.changelog.is_some() {
            if let Some((source_path, changelog_file)) = self.generate_changelog_asset(package_deb)? {
                log::debug!("added changelog via {}", source_path.display());
                package_deb.assets.resolved.push(Asset::new(
                    AssetSource::Data(changelog_file),
                    Path::new("usr/share/doc").join(&package_deb.deb_name).join("changelog.Debian.gz"),
                    0o644,
                    IsBuilt::No,
                    false,
                ).processed("generated", source_path));
            }
        }
        Ok(())
    }

    /// Generates compressed changelog file
    fn generate_changelog_asset(&self, package_deb: &PackageConfig) -> CDResult<Option<(PathBuf, Vec<u8>)>> {
        if let Some(ref path) = package_deb.changelog {
            let source_path = self.path_in_package(path);
            let changelog = fs::read(&source_path)
                .and_then(|content| {
                    // allow pre-compressed
                    if source_path.extension().is_some_and(|e| e == "gz") {
                        return Ok(content);
                    }
                    // The input is plaintext, but the debian package should contain gzipped one.
                    gzipped(&content)
                })
                .map_err(|e| CargoDebError::IoFile("unable to read changelog file", e, source_path.clone()))?;
            Ok(Some((source_path, changelog)))
        } else {
            Ok(None)
        }
    }

    fn add_systemd_assets(&self, package_deb: &mut PackageConfig) -> CDResult<()> {
        if let Some(ref config_vec) = package_deb.systemd_units {
            for config in config_vec {
                let units_dir_option = config.unit_scripts.as_ref()
                    .or(package_deb.maintainer_scripts_rel_path.as_ref());
                if let Some(unit_dir) = units_dir_option {
                    let search_path = self.path_in_package(unit_dir);
                    let package = &package_deb.name;
                    let unit_name = config.unit_name.as_deref();

                    let units = dh_installsystemd::find_units(&search_path, package, unit_name);

                    for (source, target) in units {
                        package_deb.assets.resolved.push(Asset::new(
                            AssetSource::from_path(source, package_deb.preserve_symlinks), // should this even support symlinks at all?
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

    pub(crate) fn path_in_build<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.path_in_build_(rel_path.as_ref())
    }

    pub(crate) fn path_in_build_(&self, rel_path: &Path) -> PathBuf {
        let profile = match self.build_profile_override.as_deref() {
            None => "release",
            Some("dev") => "debug",
            Some(p) => p,
        };

        let mut path = self.target_dir.join(profile);
        path.push(rel_path);
        path
    }

    pub(crate) fn path_in_package<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.package_manifest_dir.join(rel_path)
    }

    /// Store intermediate files here
    pub(crate) fn deb_temp_dir(&self, package_deb: &PackageConfig) -> PathBuf {
        self.target_dir.join("debian").join(&package_deb.name)
    }

    /// Save final .deb here
    pub(crate) fn deb_output_path(&self, package_deb: &PackageConfig) -> PathBuf {
        let filename = format!("{}_{}_{}.deb", package_deb.deb_name, package_deb.deb_version, package_deb.architecture);

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

    /// Creates empty (removes files if needed) target/debian/foo directory so that we can start fresh.
    fn reset_deb_temp_directory(&self, package_deb: &PackageConfig) -> io::Result<()> {
        let deb_temp_dir = self.deb_temp_dir(package_deb);
        let _ = fs::remove_dir(&deb_temp_dir);
        // Delete previous .deb from target/debian, but only other versions of the same package
        let mut deb_dir = self.default_deb_output_dir();
        deb_dir.push(format!("{}_*_{}.deb", package_deb.deb_name, package_deb.architecture));
        if let Ok(old_files) = glob::glob(deb_dir.to_str().ok_or(io::ErrorKind::InvalidInput)?) {
            for old_file in old_files.flatten() {
                let _ = fs::remove_file(old_file);
            }
        }
        fs::create_dir_all(deb_temp_dir)
    }
}

impl PackageConfig {
    pub(crate) fn new(mut deb: CargoDeb, cargo_package: &mut cargo_toml::Package<CargoPackageMetadata>, listener: &dyn Listener, default_timestamp: u64, overrides: DebConfigOverrides, target: Option<&str>) -> Result<Self, CargoDebError> {
        let (license_file_rel_path, license_file_skip_lines) = parse_license_file(cargo_package, deb.license_file.as_ref())?;
        let mut license = cargo_package.license.take().map(|v| v.unwrap());

        if license.is_none() && license_file_rel_path.is_none() {
            if cargo_package.publish() == false {
                license = Some("UNLICENSED".into());
                listener.info("license field defaulted to UNLICENSED".into());
            } else {
                listener.warning("license field is missing in Cargo.toml".into());
            }
        }

        let has_maintainer_override = overrides.maintainer.is_some();
        let deb_version = overrides.deb_version.unwrap_or_else(|| manifest_version_string(cargo_package, overrides.deb_revision.or(deb.revision.take()).as_deref()).into_owned());
        if let Err(why) = check_debian_version(&deb_version) {
            return Err(CargoDebError::InvalidVersion(why, deb_version));
        }
        Ok(Self {
            deb_version,
            default_timestamp,
            raw_assets: deb.assets.take(),
            name: cargo_package.name.clone(),
            deb_name: deb.name.take().unwrap_or_else(|| debian_package_name(&cargo_package.name)),
            license,
            license_file_rel_path,
            license_file_skip_lines,
            maintainer: overrides.maintainer.or_else(|| deb.maintainer.take()).ok_or_then(|| {
                Ok(cargo_package.authors().first()
                    .ok_or("The package must have a maintainer specified (--maintainer works too) or have the authors property")?.to_owned())
            })?,
            copyright: match deb.copyright.take() {
                ok @ Some(_) => ok,
                _ if !cargo_package.authors().is_empty() => Some(cargo_package.authors().join(", ")),
                _ if has_maintainer_override => {
                    // generally we'd prefer to have real authors to credit copyright to, but this is now an optional field.
                    // As a compromise if the maintainer is set on the command-line, assume they can't fix the metadata, and let it be missing.
                    None
                },
                _ => return Err("The package must have a copyright or authors property".into()),
            },
            homepage: cargo_package.homepage().map(From::from),
            documentation: cargo_package.documentation().map(From::from),
            repository: cargo_package.repository.take().map(|v| v.unwrap()),
            description: cargo_package.description.take().map_or_else(|| {
                listener.warning("description field is missing in Cargo.toml".to_owned());
                format!("[generated from Rust crate {}]", cargo_package.name)
            }, |v| v.unwrap()),
            extended_description: if let Some(path) = deb.extended_description_file.take() {
                if deb.extended_description.is_some() {
                    listener.warning("extended-description and extended-description-file are both set".into());
                }
                ExtendedDescription::File(path.into())
            } else if let Some(desc) = deb.extended_description.take() {
                ExtendedDescription::String(desc)
            } else if let Some(readme_rel_path) = cargo_package.readme().as_path() {
                if readme_rel_path.extension().is_some_and(|ext| ext == "md" || ext == "markdown") {
                    listener.info(format!("extended-description field missing. Using {}, but markdown may not render well.", readme_rel_path.display()));
                }
                ExtendedDescription::ReadmeFallback(readme_rel_path.into())
            } else {
                ExtendedDescription::None
            },
            readme_rel_path: cargo_package.readme().as_path().map(|p| p.to_path_buf()),
            wildcard_depends: deb.depends.take().map_or_else(|| "$auto".to_owned(), DependencyList::into_depends_string),
            resolved_depends: None,
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
            conf_files: deb.conf_files.take().unwrap_or_default(),
            assets: Assets::new(),
            triggers_file_rel_path: deb.triggers_file.take().map(PathBuf::from),
            changelog: deb.changelog.take(),
            maintainer_scripts_rel_path: deb.maintainer_scripts.take().map(PathBuf::from),
            preserve_symlinks: deb.preserve_symlinks.unwrap_or(false),
            systemd_units: match deb.systemd_units.take() {
                None => None,
                Some(SystemUnitsSingleOrMultiple::Single(s)) => Some(vec![s]),
                Some(SystemUnitsSingleOrMultiple::Multi(v)) => Some(v),
            },
        })
    }

    pub fn resolve_assets(&mut self) -> CDResult<()> {
        for u in self.assets.unresolved.drain(..) {
            let matched = u.resolve(self.preserve_symlinks)?;
            self.assets.resolved.extend(matched);
        }
        self.add_conf_files();
        Ok(())
    }

    /// Debian defaults all /etc files to be conf files
    /// <https://www.debian.org/doc/manuals/maint-guide/dother.en.html#conffiles>
    fn add_conf_files(&mut self) {
        let existing_conf_files = self.conf_files.iter()
            .map(|c| c.trim_start_matches('/')).collect::<HashSet<_>>();

        let mut new_conf = Vec::new();
        for a in &self.assets.resolved {
            if a.c.target_path.starts_with("etc") {
                let Some(path_str) = a.c.target_path.to_str() else { continue };
                if existing_conf_files.contains(path_str) {
                    continue;
                }
                log::debug!("automatically adding /{path_str} to conffiles");
                new_conf.push(format!("/{path_str}"));
            }
        }
        self.conf_files.append(&mut new_conf);
    }

    /// run dpkg/ldd to check deps of libs
    pub fn resolve_binary_dependencies(&mut self, target: Option<&str>, listener: &dyn Listener) -> CDResult<()> {
        let mut deps = HashSet::new();
        for word in self.wildcard_depends.split(',') {
            let word = word.trim();
            if word == "$auto" {
                let bin = self.all_binaries();
                let resolved = bin.par_iter()
                    .filter(|bin| !bin.archive_as_symlink_only())
                    .filter_map(|p| p.path())
                    .filter_map(|bname| match resolve(bname, target) {
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
                    if match_architecture(spec, &self.architecture)? {
                        deps.insert(dep);
                    }
                } else {
                    deps.insert(dep);
                }
            }
        }
        self.resolved_depends = Some(deps.into_iter().collect::<Vec<_>>().join(", "));
        Ok(())
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
                asset.c.is_built() && (asset.c.is_dynamic_library() || asset.c.is_executable())
            })
            .collect()
    }

    /// similar files next to each other improve tarball compression
    pub fn sort_assets_by_type(&mut self) {
        self.assets.resolved.sort_by(|a,b| {
            a.c.is_executable().cmp(&b.c.is_executable())
            .then(a.c.is_dynamic_library().cmp(&b.c.is_dynamic_library()))
            .then(a.processed_from.as_ref().map(|p| p.action).cmp(&b.processed_from.as_ref().map(|p| p.action)))
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

    fn extended_description(&self, config: &Config) -> CDResult<Option<Cow<'_, str>>> {
        let path = match &self.extended_description {
            ExtendedDescription::None => return Ok(None),
            ExtendedDescription::String(s) => return Ok(Some(s.as_str().into())),
            ExtendedDescription::File(p) => Cow::Borrowed(p.as_path()),
            ExtendedDescription::ReadmeFallback(p) => Cow::Owned(config.path_in_package(p)),
        };
        let desc = fs::read_to_string(&path)
            .map_err(|err| CargoDebError::IoFile("unable to read extended description from file", err, path.into_owned()))?;
        Ok(Some(desc.into()))
    }

    /// Generates the control file that obtains all the important information about the package.
    pub fn generate_control(&self, config: &Config) -> CDResult<Vec<u8>> {
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

        if let Some(deps) = &self.resolved_depends {
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

        if let Some(desc) = self.extended_description(config)? {
            for line in desc.split_by_chars(79) {
                writeln!(&mut control, " {line}")?;
            }
        }
        control.push(b'\n');

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
        if let Some(source) = self.repository.as_deref().or(self.homepage.as_deref()) {
            writeln!(copyright, "Source: {source}")?;
        }
        if let Some(c) = self.copyright.as_deref() {
            writeln!(copyright, "Copyright: {c}")?;
        }
        if let Some(license) = self.license.as_deref() {
            writeln!(copyright, "License: {license}")?;
        }
        Ok(())
    }

    pub(crate) fn conf_files(&self) -> Option<String> {
        if self.conf_files.is_empty() {
            return None;
        }
        Some(format_conffiles(&self.conf_files))
    }
}

impl TryFrom<CargoDebAssetArrayOrTable> for RawAsset {
    type Error = String;

    fn try_from(toml: CargoDebAssetArrayOrTable) -> Result<Self, Self::Error> {
        fn parse_chmod(mode: &str) -> Result<u32, String> {
            u32::from_str_radix(mode, 8).map_err(|e| format!("Unable to parse mode argument (third array element) as an octal number in an asset: {e}"))
        }
        let a = match toml {
            CargoDebAssetArrayOrTable::Table(a) => Self {
                source_path: a.source.into(), target_path: a.dest.into(), chmod: parse_chmod(&a.mode)?
            },
            CargoDebAssetArrayOrTable::Array(a) => {
                let mut a = a.into_iter();
                Self {
                    source_path: PathBuf::from(a.next().ok_or("Missing source path (first array element) in an asset in Cargo.toml")?),
                    target_path: PathBuf::from(a.next().ok_or("missing dest path (second array entry) for asset in Cargo.toml. Use something like \"usr/local/bin/\".")?),
                    chmod: parse_chmod(&a.next().ok_or("Missing mode (third array element) in an asset")?)?
                }
            },
            CargoDebAssetArrayOrTable::Invalid(bad) => {
                return Err(format!("Expected assets array to contain either an array of 3 strings, or a `{{source, dest, mode}}` object, but found: {bad}"));
            },
        };
        if a.source_path.starts_with("target/debug") {
            return Err(format!("Packaging of development-only binaries is intentionally unsupported in cargo-deb.
Please only use `target/release/` directory for built products, not `{}`.
To add debug information or additional assertions use `[profile.release]` in `Cargo.toml` instead.", a.source_path.display()));
        }
        Ok(a)
    }
}

fn parse_license_file(package: &cargo_toml::Package<CargoPackageMetadata>, license_file: Option<&LicenseFile>) -> CDResult<(Option<PathBuf>, usize)> {
    Ok(match license_file {
        Some(LicenseFile::Vec(args)) => {
            let mut args = args.iter();
            let file = args.next();
            let lines = if let Some(lines) = args.next() {
                lines.parse().map_err(|e| CargoDebError::NumParse("invalid number of lines", e))?
            } else {0};
            (file.map(|s|s.into()), lines)
        },
        Some(LicenseFile::String(s)) => (Some(s.into()), 0),
        None => (package.license_file().as_ref().map(|s| s.into()), 0),
    })
}

fn has_copyright_metadata(file: &str) -> bool {
    file.lines().take(10)
        .any(|l| ["Copyright: ", "License: ", "Source: ", "Upstream-Name: ", "Format: "].into_iter().any(|f| l.starts_with(f)))
}

/// Debian doesn't like `_` in names
fn debian_package_name(crate_name: &str) -> String {
    // crate names are ASCII only
    crate_name.bytes().map(|c| {
        if c != b'_' {c.to_ascii_lowercase() as char} else {'-'}
    }).collect()
}

impl Config {
    fn explicit_assets(&self, assets: Vec<RawAsset>) -> CDResult<Assets> {
        let custom_profile_target_dir = self.build_profile_override.as_deref().map(|profile| format!("target/{profile}"));
        // Treat all explicit assets as unresolved until after the build step
        let unresolved_assets = assets.into_iter().map(|RawAsset { source_path, target_path, chmod }| {
            // target/release is treated as a magic prefix that resolves to any profile
            let target_artifact_rel_path = source_path.strip_prefix("target/release").ok()
                .or_else(|| source_path.strip_prefix(custom_profile_target_dir.as_ref()?).ok());
            let (is_built, source_path, is_example) = if let Some(rel_path) = target_artifact_rel_path {
                let is_example = rel_path.starts_with("examples");

                (self.find_is_built_file_in_package(rel_path, if is_example { "example" } else { "bin" }), self.path_in_build(rel_path), is_example)
            } else {
                (IsBuilt::No, self.path_in_package(&source_path), false)
            };
            Ok(UnresolvedAsset::new(source_path, target_path, chmod, is_built, is_example))
        }).collect::<CDResult<Vec<_>>>()?;
        Ok(Assets::with_unresolved_assets(unresolved_assets))
    }

    fn implicit_assets(&self, deb_package_name: &str, readme_rel_path: Option<&Path>) -> CDResult<Assets> {
        let mut implied_assets: Vec<_> = self.build_targets.iter()
            .filter_map(|t| {
                if t.crate_types.iter().any(|ty| ty == "bin") && t.kind.iter().any(|k| k == "bin") {
                    Some(Asset::new(
                        AssetSource::Path(self.path_in_build(&t.name)),
                        Path::new("usr/bin").join(&t.name),
                        0o755,
                        self.is_built_file_in_package(t),
                        false,
                    ))
                } else if t.crate_types.iter().any(|ty| ty == "cdylib") && t.kind.iter().any(|k| k == "cdylib") {
                    // FIXME: std has constants for the host arch, but not for cross-compilation
                    let lib_name = format!("{DLL_PREFIX}{}{DLL_SUFFIX}", t.name);
                    Some(Asset::new(
                        AssetSource::Path(self.path_in_build(&lib_name)),
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
        if implied_assets.is_empty() {
            return Err("No binaries or cdylibs found. The package is empty. Please specify some assets to package in Cargo.toml".into());
        }
        if let Some(readme_rel_path) = readme_rel_path {
            let path = self.path_in_package(readme_rel_path);
            let target_path = Path::new("usr/share/doc")
                .join(deb_package_name)
                .join(path.file_name().ok_or("bad README path")?);
            implied_assets.push(Asset::new(AssetSource::Path(path), target_path, 0o644, IsBuilt::No, false));
        }
        Ok(Assets::with_resolved_assets(implied_assets))
    }

    fn find_is_built_file_in_package(&self, rel_path: &Path, expected_kind: &str) -> IsBuilt {
        let source_name = rel_path.file_name().expect("asset filename").to_str().expect("utf-8 names");
        let source_name = source_name.strip_suffix(EXE_SUFFIX).unwrap_or(source_name);

        if self.build_targets.iter()
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

fn check_debian_version(mut ver: &str) -> Result<(), &'static str> {
    if ver.trim_start().is_empty() {
        return Err("empty string");
    }

    if let Some((epoch, ver_rest)) = ver.split_once(':') {
        ver = ver_rest;
        if epoch.is_empty() || epoch.as_bytes().iter().any(|c| !c.is_ascii_digit()) {
            return Err("version has unexpected ':' char");
        }
    }

    if !ver.starts_with(|c: char| c.is_ascii_digit()) {
        return Err("version must start with a digit");
    }

    if ver.as_bytes().iter().any(|&c| !c.is_ascii_alphanumeric() && !matches!(c, b'.' | b'+' | b'-' | b'~')) {
        return Err("contains characters other than a-z 0-9 . + - ~");
    }
    Ok(())
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
            ("libjpeg64-turbo".to_owned(), Some(Require("armhf".to_owned())))
        );
        // neg
        assert_eq!(
            get_architecture_specification("libjpeg64-turbo [!amd64]").expect("arch"),
            ("libjpeg64-turbo".to_owned(), Some(NegRequire("amd64".to_owned())))
        );
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

        let (config, mut package_deb) = Config::from_manifest(Some(Path::new("Cargo.toml")), None, None, None, None, DebConfigOverrides::default(), None, None, None, CargoLockingFlags::default(), &mock_listener).unwrap();
        config.prepare_assets_before_build(&mut package_deb).unwrap();

        let num_unit_assets = package_deb.assets.resolved.iter()
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

        let (config, mut package_deb) = Config::from_manifest(Some(Path::new("Cargo.toml")), None, None, None, None, DebConfigOverrides::default(), None, None, None, CargoLockingFlags::default(), &mock_listener).unwrap();
        config.prepare_assets_before_build(&mut package_deb).unwrap();

        package_deb.systemd_units.get_or_insert(vec![SystemdUnitsConfig::default()]);
        package_deb.maintainer_scripts_rel_path.get_or_insert(PathBuf::new());

        config.add_systemd_assets(&mut package_deb).unwrap();

        let num_unit_assets = package_deb.assets.resolved
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
