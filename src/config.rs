use crate::assets::{AssetFmt, AssetKind, RawAssetOrAuto, Asset, AssetSource, Assets, IsBuilt, UnresolvedAsset, RawAsset};
use crate::assets::is_dynamic_library_filename;
use crate::util::compress::gzipped;
use crate::dependencies::resolve_with_dpkg;
use crate::dh::dh_installsystemd;
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::parse::cargo::CargoConfig;
use crate::parse::manifest::{cargo_metadata, debug_flags, find_profile, manifest_version_string};
use crate::parse::manifest::{CargoDeb, CargoDebAssetArrayOrTable, CargoMetadataTarget, CargoPackageMetadata, ManifestFound};
use crate::parse::manifest::{DependencyList, SystemUnitsSingleOrMultiple, SystemdUnitsConfig, LicenseFile, ManifestDebugFlags};
use crate::util::wordsplit::WordSplit;
use crate::{debian_architecture_from_rust_triple, debian_triple_from_rust_triple, CargoLockingFlags, DEFAULT_TARGET};
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::env::consts::{DLL_PREFIX, DLL_SUFFIX, EXE_SUFFIX};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use std::{fmt, fs, io, mem};

pub(crate) fn is_glob_pattern(s: impl AsRef<Path>) -> bool {
    // glob crate requires str anyway ;(
    s.as_ref().to_str().map_or(false, |s| s.as_bytes().iter().any(|&c| c == b'*' || c == b'[' || c == b']' || c == b'!'))
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
pub struct BuildEnvironment {
    /// Directory where `Cargo.toml` is located. It's a subdirectory in workspaces.
    pub package_manifest_dir: PathBuf,
    /// Run `cargo` commands from this dir, or things may subtly break
    pub cargo_run_current_dir: PathBuf,
    /// User-configured output path for *.deb
    pub deb_output_path: Option<String>,
    /// Triple. `None` means current machine architecture.
    pub rust_target_triple: Option<String>,
    /// `CARGO_TARGET_DIR`
    pub target_dir: PathBuf,
    /// List of Cargo features to use during build
    pub features: Vec<String>,
    pub default_features: bool,
    pub all_features: bool,
    /// Should the binary be stripped from debug symbols?
    pub debug_symbols: DebugSymbols,

    build_profile: BuildProfile,

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
    pub cargo_crate_name: String,
    /// The name to give the Debian package; usually the same as the Cargo project name
    pub deb_name: String,
    /// The version to give the Debian package; usually the same as the Cargo version
    pub deb_version: String,
    /// The software license of the project (SPDX format).
    pub license_identifier: Option<String>,
    /// The location of the license file
    pub license_file_rel_path: Option<PathBuf>,
    /// number of lines to skip when reading `license_file`
    pub license_file_skip_lines: usize,
    /// Names of copyright owners (credit in `Copyright` metadata)
    /// Used in Debian's `copyright` file, which is *required* by Debian.
    pub copyright: Option<String>,
    pub changelog: Option<String>,
    /// The homepage URL of the project.
    pub homepage: Option<String>,
    /// Documentation URL from `Cargo.toml`. Fallback if `homepage` is missing.
    pub documentation: Option<String>,
    /// The URL of the software repository. Fallback if both `homepage` and `documentation` are missing.
    pub repository: Option<String>,
    /// A short description of the project.
    pub description: String,
    /// An extended description of the project.
    pub extended_description: ExtendedDescription,
    /// The maintainer of the Debian package.
    /// In Debian `control` file `Maintainer` field format.
    pub maintainer: Option<String>,
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
    /// Support Debian's multiarch, which puts libs in `/usr/lib/$tuple/`
    pub multiarch: Multiarch,
    /// A list of configuration files installed by the package.
    /// Automatically includes all files in `/etc`
    pub conf_files: Vec<String>,
    /// All of the files that are to be packaged.
    pub(crate) assets: Assets,

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
    /// Save it under a different path
    pub is_split_dbgsym_package: bool,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum DebugSymbols {
    /// No change (also used if Cargo already stripped the symbols
    Keep,
    Strip,
    /// Should the debug symbols be moved to a separate file included in the package? (implies `strip:true`)
    Separate {
        /// Should the debug symbols be compressed
        compress: bool,
        /// Generate dbgsym.ddeb package
        generate_dbgsym_package: bool,
    },
}

/// Replace config values via command-line
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct DebConfigOverrides {
    pub deb_version: Option<String>,
    pub deb_revision: Option<String>,
    pub maintainer: Option<String>,
    pub section: Option<String>,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub all_features: bool,
    pub(crate) systemd_units: Option<Vec<SystemdUnitsConfig>>,
    pub(crate) maintainer_scripts_rel_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct BuildProfile {
    /// "release" by default
    pub profile_name: Option<String>,
    /// Cargo setting
    pub override_debug: Option<String>,
    pub override_lto: Option<String>,
}

impl BuildProfile {
    #[must_use]
    pub fn profile_name(&self) -> &str {
        self.profile_name.as_deref().unwrap_or("release")
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum Multiarch {
    /// Not supported
    #[default]
    None,
    /// Architecture-dependent, but more than one arch can be installed at the same time
    Same,
    /// For architecture-independent tools
    Foreign,
}

#[derive(Debug, Default)]
pub struct DebugSymbolOptions {
    pub generate_dbgsym_package: Option<bool>,
    pub separate_debug_symbols: Option<bool>,
    pub compress_debug_symbols: Option<bool>,
    pub strip_override: Option<bool>,
}

#[derive(Debug, Default)]
pub struct BuildOptions<'a> {
    pub manifest_path: Option<&'a Path>,
    pub selected_package_name: Option<&'a str>,
    pub deb_output_path: Option<String>,
    pub rust_target_triple: Option<&'a str>,
    pub config_variant: Option<&'a str>,
    pub overrides: DebConfigOverrides,
    pub build_profile: BuildProfile,
    pub debug: DebugSymbolOptions,
    pub cargo_locking_flags: CargoLockingFlags,
    pub multiarch: Multiarch,
}

impl BuildEnvironment {
    /// Makes a new config from `Cargo.toml` in the `manifest_path`
    ///
    /// `None` target means the host machine's architecture.
    pub fn from_manifest(
        BuildOptions {
            manifest_path,
            selected_package_name,
            deb_output_path,
            rust_target_triple,
            config_variant,
            overrides,
            build_profile,
            debug,
            cargo_locking_flags,
            multiarch,
        }: BuildOptions,
        listener: &dyn Listener,
    ) -> CDResult<(Self, PackageConfig)> {
        // **IMPORTANT**: This function must not create or expect to see any asset files on disk!
        // It's run before destination directory is cleaned up, and before the build start!

        let ManifestFound {
            build_targets,
            root_manifest,
            workspace_root_manifest_path,
            mut manifest_path,
            mut target_dir,
            mut manifest,
        } = cargo_metadata(manifest_path, selected_package_name, cargo_locking_flags)?;

        let default_timestamp = if let Ok(source_date_epoch) = std::env::var("SOURCE_DATE_EPOCH") {
            source_date_epoch.parse().map_err(|e| CargoDebError::NumParse("SOURCE_DATE_EPOCH", e))?
        } else {
            let manifest_mdate = fs::metadata(&manifest_path)?.modified().unwrap_or_else(|_| SystemTime::now());
            let mut timestamp = manifest_mdate.duration_since(SystemTime::UNIX_EPOCH).map_err(CargoDebError::SystemTime)?.as_secs();
            timestamp -= timestamp % (24 * 3600);
            timestamp
        };

        // Cargo cross-compiles to a dir
        if let Some(rust_target_triple) = rust_target_triple {
            target_dir.push(rust_target_triple);
        }

        let selected_profile = build_profile.profile_name();

        let package_profile = find_profile(&manifest, selected_profile);
        let root_profile = root_manifest.as_ref().and_then(|m| find_profile(m, selected_profile));
        if package_profile.is_some() && root_profile.is_some() {
            let rel_path = workspace_root_manifest_path.parent().and_then(|base| manifest_path.strip_prefix(base).ok()).unwrap_or(&manifest_path);
            listener.warning(format!("The [profile.{selected_profile}] is in both the package and the root workspace.\n\
                Picking root ({}) over the package ({}) for compatibility with Cargo", workspace_root_manifest_path.display(), rel_path.display()));
        }

        let manifest_debug = root_profile.or(package_profile)
            .map(|profile_toml| debug_flags(profile_toml, &build_profile))
            .unwrap_or(ManifestDebugFlags::Default);

        drop(workspace_root_manifest_path);
        drop(root_manifest);

        let cargo_package = manifest.package.as_mut().ok_or("Cargo.toml is a workspace, not a package")?;

        // If we build against a variant use that config and change the package name
        let mut deb = if let Some(variant) = config_variant {
            let mut deb = cargo_package.metadata.take()
                .and_then(|m| m.deb).unwrap_or_default();
            if deb.name.is_none() {
                deb.name = Some(debian_package_name(&format!("{}-{variant}", cargo_package.name)));
            }
            deb.variants
                .as_mut()
                .and_then(|v| v.remove(variant))
                .ok_or_else(|| CargoDebError::VariantNotFound(variant.to_string()))?
                .inherit_from(deb, listener)
        } else {
            cargo_package.metadata.take().and_then(|m| m.deb).unwrap_or_default()
        };

        let debug_symbols = Self::configure_debug_symbols(debug, &deb, manifest_debug, selected_profile, listener);

        let mut features = deb.features.take().unwrap_or_default();
        features.extend(overrides.features.iter().cloned());

        manifest_path.pop();
        let manifest_dir = manifest_path;

        let config = Self {
            package_manifest_dir: manifest_dir,
            deb_output_path,
            rust_target_triple: rust_target_triple.map(|t| t.to_string()),
            target_dir,
            features,
            all_features: overrides.all_features,
            default_features: if overrides.no_default_features { false } else { deb.default_features.unwrap_or(true) },
            debug_symbols,
            build_profile,
            build_targets,
            cargo_locking_flags,
            cargo_run_current_dir: std::env::current_dir().unwrap_or_default(),
        };

        let arch = debian_architecture_from_rust_triple(config.rust_target_triple());
        let assets = deb.assets.take().unwrap_or_else(|| vec![RawAssetOrAuto::Auto]);
        let mut package_deb = PackageConfig::new(deb, cargo_package, listener, default_timestamp, overrides, arch, multiarch)?;

        config.add_assets(&mut package_deb, assets, listener)?;

        Ok((config, package_deb))
    }

    fn configure_debug_symbols(debug: DebugSymbolOptions, deb: &CargoDeb, manifest_debug: ManifestDebugFlags, selected_profile: &str, listener: &dyn Listener) -> DebugSymbols {
        let DebugSymbolOptions { generate_dbgsym_package, separate_debug_symbols, compress_debug_symbols, strip_override } = debug;
        let allows_strip = strip_override != Some(false);
        let allows_separate_debug_symbols = separate_debug_symbols != Some(false);

        let generate_dbgsym_package = generate_dbgsym_package
            .or((!allows_strip).then_some(false)) // --no-strip means not running the strip command, even to separate symbols
            .or(deb.dbgsym)
            .unwrap_or(allows_separate_debug_symbols && crate::DBGSYM_DEFAULT);
        let wants_separate_debug_symbols = separate_debug_symbols
            .or((!allows_strip).then_some(false)) // --no-strip means not running the strip command, even to separate symbols
            .or(deb.separate_debug_symbols)
            .unwrap_or(generate_dbgsym_package || (allows_separate_debug_symbols && crate::SEPARATE_DEBUG_SYMBOLS_DEFAULT));
        let separate_debug_symbols = generate_dbgsym_package || wants_separate_debug_symbols;
        let compress_debug_symbols = compress_debug_symbols.or(deb.compress_debug_symbols).unwrap_or(false);

        let separate_option_name = if generate_dbgsym_package { "dbgsym" } else { "separate-debug-symbols" };
        if !allows_strip && separate_debug_symbols {
            listener.warning(format!("--no-strip has no effect when using {separate_option_name}"));
        }
        else if generate_dbgsym_package && !wants_separate_debug_symbols {
            listener.warning("separate-debug-symbols can't be disabled when generating dbgsym".into());
        }
        else if !separate_debug_symbols && compress_debug_symbols {
            listener.warning("--separate-debug-symbols or --dbgsym is required to compresss symbols".into());
        }

        let strip_override_default = strip_override.map(|s| if s { DebugSymbols::Strip } else { DebugSymbols::Keep });

        let keep_debug_symbols_default = if separate_debug_symbols {
            DebugSymbols::Separate {
                compress: compress_debug_symbols,
                generate_dbgsym_package,
            }
        } else {
            strip_override_default.unwrap_or(DebugSymbols::Keep)
        };

        let debug_symbols = match manifest_debug {
            ManifestDebugFlags::SomeSymbolsAdded => keep_debug_symbols_default,
            ManifestDebugFlags::FullSymbolsAdded => {
                if !separate_debug_symbols {
                    listener.warning(format!("the debug symbols may be bloated\nUse `[profile.{selected_profile}] debug = \"line-tables-only\"` or --separate-debug-symbols or --dbgsym options"));
                }
                keep_debug_symbols_default
            },
            ManifestDebugFlags::Default if separate_debug_symbols => {
                listener.warning(format!("debug info hasn't been explicitly enabled, so {separate_option_name} may not work\nAdd `[profile.{selected_profile}] debug = 1` to Cargo.toml"));
                keep_debug_symbols_default
            },
            ManifestDebugFlags::FullyStrippedByCargo => {
                if separate_debug_symbols || compress_debug_symbols {
                    listener.warning(format!("{separate_option_name} won't have any effect when Cargo is configured to strip the symbols first.\nRemove `strip` from `[profile.{selected_profile}]`"));
                }
                strip_override_default.unwrap_or(DebugSymbols::Keep) // no need to launch strip
            },
            ManifestDebugFlags::SymbolsDisabled => {
                if separate_debug_symbols || generate_dbgsym_package {
                    listener.warning(format!("{separate_option_name} won't have any effect when debug symbols are disabled\nAdd `[profile.{selected_profile}] debug = \"line-tables-only\"` to Cargo.toml"));
                }
                // Rust still adds debug bloat from the libstd
                strip_override_default.unwrap_or(DebugSymbols::Strip)
            },
            ManifestDebugFlags::Default => {
                // Rust still adds debug bloat from the libstd
                strip_override_default.unwrap_or(DebugSymbols::Strip)
            },
            ManifestDebugFlags::SymbolsPackedExternally => {
                listener.warning("Cargo's split-debuginfo option (.dwp/.dwo) is not supported; the symbols may be incomplete".into());
                keep_debug_symbols_default
            },
        };
        debug_symbols
    }

    fn add_assets(&self, package_deb: &mut PackageConfig, assets: Vec<RawAssetOrAuto>, listener: &dyn Listener) -> CDResult<()> {
        package_deb.assets = self.explicit_assets(package_deb, assets, listener)?;

        // https://wiki.debian.org/Multiarch/Implementation
        if package_deb.multiarch != Multiarch::None {
            let mut has_bin = None;
            let mut has_lib = None;
            let multiarch_lib_dir_prefix = &package_deb.multiarch_lib_dirs(self.rust_target_triple())[0];
            for c in package_deb.assets.iter() {
                let p = c.target_path.as_path();
                if has_bin.is_none() && (p.starts_with("bin") || p.starts_with("usr/bin") || p.starts_with("usr/sbin")) {
                    has_bin = Some(p);
                } else if has_lib.is_none() && p.starts_with(multiarch_lib_dir_prefix) {
                    has_lib = Some(p);
                }
                if let Some((lib, bin)) = has_lib.zip(has_bin) {
                    listener.warning(format!("Multiarch packages are not allowed to contain both libs and binaries.\n'{}' and '{}' can't be in the same package.", lib.display(), bin.display()));
                    break;
                }
            }
        }

        self.add_copyright_asset(package_deb, listener)?;
        self.add_changelog_asset(package_deb)?;
        self.add_systemd_assets(package_deb, listener)?;

        self.reset_deb_temp_directory(package_deb)?;
        Ok(())
    }

    pub fn set_cargo_build_flags_for_package<'s>(&'s self, package_deb: &PackageConfig, flags: &mut Vec<Cow<'s, OsStr>>, env: &mut Vec<(Cow<'s, OsStr>, Cow<'s, OsStr>)>) {
        let flags_already_build_a_workspace = flags.iter().any(|f| &**f == "--workspace" || &**f == "--all");
        fn s(s: &(impl AsRef<OsStr> + ?Sized)) -> Cow<'_, OsStr> {
            Cow::Borrowed(s.as_ref())
        }
        fn o<'s>(s: impl Into<String>) -> Cow<'s, OsStr> {
            Cow::Owned(OsString::from(s.into()))
        }

        let profile_name = self.build_profile.profile_name();

        for (name, val) in [("DEBUG", &self.build_profile.override_debug), ("LTO", &self.build_profile.override_lto)] {
            if let Some(val) = val {
                env.push((o(format!("CARGO_PROFILE_{}_{name}", profile_name.to_ascii_uppercase())), s(val)));
            }
        }

        flags.push(if profile_name == "release" { s("--release") } else { o(format!("--profile={profile_name}")) });
        flags.extend(self.cargo_locking_flags.flags().map(|f| s(f)));

        if let Some(rust_target_triple) = self.rust_target_triple.as_deref() {
            flags.extend([s("--target"), o(rust_target_triple)]);
            // Set helpful defaults for cross-compiling
            if std::env::var_os("PKG_CONFIG_ALLOW_CROSS").is_none() && std::env::var_os("PKG_CONFIG_PATH").is_none() {
                let pkg_config_path = format!("/usr/lib/{}/pkgconfig", debian_triple_from_rust_triple(rust_target_triple));
                if Path::new(&pkg_config_path).exists() {
                    env.push((s("PKG_CONFIG_ALLOW_CROSS"), s("1")));
                    env.push((s("PKG_CONFIG_PATH"), o(pkg_config_path)));
                }
            }
        }

        if self.all_features {
            flags.push(s("--all-features"));
        } else  if !self.default_features {
            flags.push(s("--no-default-features"));
        }
        if !self.features.is_empty() {
            flags.extend([s("--features"), o(self.features.join(","))]);
        }

        if flags_already_build_a_workspace {
            return;
        }

        for a in package_deb.assets.unresolved.iter().filter(|a| a.c.is_built()) {
            if is_glob_pattern(&a.source_path) {
                log::debug!("building entire workspace because of glob {}", a.source_path.display());
                flags.push(s("--workspace"));
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
            if asset_target.is_dynamic_library() || source_path.is_some_and(is_dynamic_library_filename) {
                log::debug!("building libs for {}", source_path.unwrap_or(&asset_target.target_path).display());
                build_libs = true;
            } else if asset_target.is_executable() {
                if let Some(source_path) = source_path {
                    let name = source_path.file_name().unwrap().to_str().expect("utf-8 target name");
                    let name = name.strip_suffix(EXE_SUFFIX).unwrap_or(name);
                    if asset_target.asset_kind == AssetKind::CargoExampleBinary {
                        build_examples.push(name);
                    } else {
                        build_bins.push(name);
                    }
                }
            }
        }

        if !same_package {
            flags.push(s("--workspace"));
        }
        flags.extend(build_bins.iter().map(|&name| {
            log::debug!("building bin for {name}");
            o(format!("--bin={name}"))
        }));
        flags.extend(build_examples.iter().map(|&name| {
            log::debug!("building example for {name}");
            o(format!("--example={name}"))
        }));
        if build_libs {
            flags.push(s("--lib"));
        }
    }

    fn add_copyright_asset(&self, package_deb: &mut PackageConfig, listener: &dyn Listener) -> CDResult<()> {
        let destination_path = Path::new("usr/share/doc").join(&package_deb.deb_name).join("copyright");
        if package_deb.assets.iter().any(|a| a.target_path == destination_path) {
            listener.info(format!("Not generating a default copyright, because asset for {} exists", destination_path.display()));
            return Ok(());
        }

        let (source_path, (copyright_file, incomplete)) = self.generate_copyright_asset(package_deb)?;
        if incomplete {
            listener.warning("Debian requires copyright information, but the Cargo package doesn't have it.\n\
                Use --maintainer flag to skip this warning.\n\
                Otherwise, edit Cargo.toml to add `[package] authors = [\"...\"]`, or \n\
                `[package.metadata.deb] copyright = \"© copyright owner's name\"`.\n\
                If the package is proprietary, add `[package] license = \"UNLICENSED\"` or `publish = false`.\n\
                You can also specify `license-file = \"path\"` to a Debian-formatted `copyright` file.".into());
        }
        log::debug!("added copyright via {}", source_path.display());
        package_deb.assets.resolved.push(Asset::new(
            AssetSource::Data(copyright_file.into()),
            destination_path,
            0o644,
            IsBuilt::No,
            AssetKind::Any,
        ).processed("generated", source_path));
        Ok(())
    }

    /// Generates the copyright file from the license file and adds that to the tar archive.
    fn generate_copyright_asset(&self, package_deb: &PackageConfig) -> CDResult<(PathBuf, (String, bool))> {
        Ok(if let Some(path) = &package_deb.license_file_rel_path {
            let source_path = self.path_in_package(path);
            let license_string = fs::read_to_string(&source_path)
                .map_err(|e| CargoDebError::IoFile("unable to read license file", e, path.clone()))?;

            let (mut copyright, incomplete) = if has_copyright_metadata(&license_string) {
                (String::new(), false)
            } else {
                package_deb.write_copyright_metadata(true)?
            };

            // Skip the first `A` number of lines and then iterate each line after that.
            for line in license_string.lines().skip(package_deb.license_file_skip_lines) {
                // If the line is a space, add a dot, else write the line.
                if line == " " {
                    copyright.push_str(" .\n");
                } else {
                    copyright.push_str(line);
                    copyright.push('\n');
                }
            }
            (source_path, (copyright, incomplete))
        } else {
            ("Cargo.toml".into(), package_deb.write_copyright_metadata(false)?)
        })
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
                    AssetKind::Any,
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

    fn add_systemd_assets(&self, package_deb: &mut PackageConfig, listener: &dyn Listener) -> CDResult<()> {
        if let Some(ref config_vec) = package_deb.systemd_units {
            for config in config_vec {
                let units_dir_option = config.unit_scripts.as_ref()
                    .or(package_deb.maintainer_scripts_rel_path.as_ref());
                if let Some(unit_dir) = units_dir_option {
                    let search_path = self.path_in_package(unit_dir);
                    let unit_name = config.unit_name.as_deref();

                    let mut units = dh_installsystemd::find_units(&search_path, &package_deb.deb_name, unit_name);
                    if package_deb.deb_name != package_deb.cargo_crate_name {
                        let fallback_units = dh_installsystemd::find_units(&search_path, &package_deb.cargo_crate_name, unit_name);
                        if !fallback_units.is_empty() && fallback_units != units {
                            let unit_name_info = unit_name.unwrap_or("<unit_name unspecified>");
                            if units.is_empty() {
                                units = fallback_units;
                                listener.warning(format!("Systemd unit {unit_name_info} found for Cargo package name ({}), but Debian package name was expected ({}). Used Cargo package name as a fallback.", package_deb.cargo_crate_name, package_deb.deb_name));
                            } else {
                                listener.warning(format!("Cargo package name and Debian package name are different ({} !=  {}) and both have systemd units. Used Debian package name for the systemd unit {unit_name_info}.", package_deb.cargo_crate_name, package_deb.deb_name));
                            }
                        }
                    }

                    for (source, target) in units {
                        package_deb.assets.resolved.push(Asset::new(
                            AssetSource::from_path(source, package_deb.preserve_symlinks), // should this even support symlinks at all?
                            target.path,
                            target.mode,
                            IsBuilt::No,
                            AssetKind::Any,
                        ).processed("systemd", unit_dir.clone()));
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
        let mut path = self.target_dir.join(match self.build_profile.profile_name() {
            "dev" => "debug",
            p => p,
        });
        path.push(rel_path);
        path
    }

    pub(crate) fn path_in_package<P: AsRef<Path>>(&self, rel_path: P) -> PathBuf {
        self.package_manifest_dir.join(rel_path)
    }

    /// Store intermediate files here
    pub(crate) fn deb_temp_dir(&self, package_deb: &PackageConfig) -> PathBuf {
        self.target_dir.join("debian").join(&package_deb.cargo_crate_name)
    }

    /// Save final .deb here
    pub(crate) fn deb_output_path(&self, package_deb: &PackageConfig) -> PathBuf {
        let filename = format!(
            "{}_{}_{}.{}",
            package_deb.deb_name,
            package_deb.deb_version,
            package_deb.architecture,
            if package_deb.is_split_dbgsym_package {
                "ddeb"
            } else {
                "deb"
            }
        );

        if let Some(ref path_str) = self.deb_output_path {
            let path = Path::new(path_str);
            if path_str.ends_with('/') || path.is_dir() {
                path.join(filename)
            } else if package_deb.is_split_dbgsym_package {
                path.with_extension("ddeb")
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
        CargoConfig::new(&self.cargo_run_current_dir)
    }

    /// Creates empty (removes files if needed) target/debian/foo directory so that we can start fresh.
    fn reset_deb_temp_directory(&self, package_deb: &PackageConfig) -> io::Result<()> {
        let deb_temp_dir = self.deb_temp_dir(package_deb);
        let _ = fs::remove_dir(&deb_temp_dir);
        // Delete previous .deb from target/debian, but only other versions of the same package
        let deb_dir = self.default_deb_output_dir();
        for base_name in [
            format!("{}_*_{}.deb", package_deb.deb_name, package_deb.architecture),
            format!("{}-dbgsym_*_{}.ddeb", package_deb.deb_name, package_deb.architecture),
        ] {
            if let Ok(old_files) = glob::glob(deb_dir.join(base_name).to_str().ok_or(io::ErrorKind::InvalidInput)?) {
                for old_file in old_files.flatten() {
                    let _ = fs::remove_file(old_file);
                }
            }
        }
        fs::create_dir_all(deb_temp_dir)
    }

    #[must_use]
    pub fn rust_target_triple(&self) -> &str {
        self.rust_target_triple.as_deref().unwrap_or(DEFAULT_TARGET)
    }
}

impl PackageConfig {
    pub(crate) fn new(
        mut deb: CargoDeb, cargo_package: &mut cargo_toml::Package<CargoPackageMetadata>, listener: &dyn Listener, default_timestamp: u64,
        overrides: DebConfigOverrides, architecture: &str, multiarch: Multiarch,
    ) -> Result<Self, CargoDebError> {
        let (license_file_rel_path, license_file_skip_lines) = parse_license_file(cargo_package, deb.license_file.as_ref())?;
        let mut license_identifier = cargo_package.license.take().and_then(|mut v| v.get_mut().ok().map(mem::take));

        if license_identifier.is_none() && license_file_rel_path.is_none() {
            if cargo_package.publish() == false {
                license_identifier = Some("UNLICENSED".into());
                listener.info("license field defaulted to UNLICENSED".into());
            } else {
                listener.warning("license field is missing in Cargo.toml".into());
            }
        }
        let deb_version = overrides.deb_version.unwrap_or_else(|| manifest_version_string(cargo_package, overrides.deb_revision.or(deb.revision.take()).as_deref()).into_owned());
        if let Err(why) = check_debian_version(&deb_version) {
            return Err(CargoDebError::InvalidVersion(why, deb_version));
        }
        Ok(Self {
            deb_version,
            default_timestamp,
            cargo_crate_name: cargo_package.name.clone(),
            deb_name: deb.name.take().unwrap_or_else(|| debian_package_name(&cargo_package.name)),
            license_identifier,
            license_file_rel_path,
            license_file_skip_lines,
            maintainer: overrides.maintainer.or_else(|| deb.maintainer.take()).or_else(|| cargo_package.authors().first().map(String::from)),
            copyright: deb.copyright.take().or_else(|| (!cargo_package.authors().is_empty()).then_some(cargo_package.authors().join(", "))),
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
            enhances: deb.enhances.take().map(DependencyList::into_depends_string),
            conflicts: deb.conflicts.take().map(DependencyList::into_depends_string),
            breaks: deb.breaks.take().map(DependencyList::into_depends_string),
            replaces: deb.replaces.take().map(DependencyList::into_depends_string),
            provides: deb.provides.take().map(DependencyList::into_depends_string),
            section: overrides.section.or_else(|| deb.section.take()),
            priority: deb.priority.take().unwrap_or_else(|| "optional".to_owned()),
            architecture: architecture.to_owned(),
            conf_files: deb.conf_files.take().unwrap_or_default(),
            assets: Assets::new(vec![], vec![]),
            triggers_file_rel_path: deb.triggers_file.take().map(PathBuf::from),
            changelog: deb.changelog.take(),
            maintainer_scripts_rel_path: overrides.maintainer_scripts_rel_path
                .or_else(|| deb.maintainer_scripts.take().map(PathBuf::from)),
            preserve_symlinks: deb.preserve_symlinks.unwrap_or(false),
            systemd_units: overrides.systemd_units.or_else(|| match deb.systemd_units.take() {
                None => None,
                Some(SystemUnitsSingleOrMultiple::Single(s)) => Some(vec![s]),
                Some(SystemUnitsSingleOrMultiple::Multi(v)) => Some(v),
            }),
            multiarch,
            is_split_dbgsym_package: false,
        })
    }

    /// Use `/usr/lib/arch-linux-gnu` dir for libraries
    pub fn set_multiarch(&mut self, enable: Multiarch) {
        self.multiarch = enable;
    }

    pub(crate) fn library_install_dir(&self, rust_target_triple: &str) -> Cow<'static, Path> {
        if self.multiarch == Multiarch::None {
            Path::new("usr/lib").into()
        } else {
            let [p, _] = self.multiarch_lib_dirs(rust_target_triple);
            p.into()
        }
    }

    /// Apparently, Debian uses both! The first one is preferred?
    pub(crate) fn multiarch_lib_dirs(&self, rust_target_triple: &str) -> [PathBuf; 2] {
        let triple = debian_triple_from_rust_triple(rust_target_triple);
        let debian_multiarch = PathBuf::from(format!("usr/lib/{triple}"));
        let gcc_crossbuild = PathBuf::from(format!("usr/{triple}/lib"));
        [debian_multiarch, gcc_crossbuild]
    }

    pub fn resolve_assets(&mut self, listener: &dyn Listener) -> CDResult<()> {
        let cwd = std::env::current_dir().unwrap_or_default();

        let unresolved = std::mem::take(&mut self.assets.unresolved);
        let matched = unresolved.into_par_iter().map(|asset| {
            asset.resolve(self.preserve_symlinks).map_err(|e| e.context(format_args!("Can't resolve asset: {}", AssetFmt::unresolved(&asset, &cwd))))
        }).collect_vec_list();
        for res in matched.into_iter().flatten() {
            self.assets.resolved.extend(res?);
        }

        let mut target_paths = HashMap::new();
        let mut indices_to_remove = Vec::new();
        for (idx, asset) in self.assets.resolved.iter().enumerate() {
            target_paths.entry(asset.c.target_path.as_path()).and_modify(|&mut old_asset| {
                listener.warning(format!("Duplicate assets: [{}] and [{}] have the same target path; first one wins", AssetFmt::new(old_asset, &cwd), AssetFmt::new(asset, &cwd)));
                indices_to_remove.push(idx);
            }).or_insert(asset);
        }
        for idx in indices_to_remove.into_iter().rev() {
            self.assets.resolved.swap_remove(idx);
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
    pub fn resolved_binary_dependencies(&self, rust_target_triple: Option<&str>, listener: &dyn Listener) -> CDResult<String> {
        // When cross-compiling, resolve dependencies using libs for the target platform (where multiarch is supported)
        let lib_search_paths = rust_target_triple.map(|triple| self.multiarch_lib_dirs(triple));
        let lib_search_paths: Vec<_> = lib_search_paths.iter().flatten().enumerate()
            .filter_map(|(i, dir)| {
                if dir.exists() {
                    Some(dir.as_path())
                } else {
                    if i == 0 { // report only the preferred one
                        log::debug!("lib dir doesn't exist: {}", dir.display());
                    }
                    None
                }
            })
            .collect();

        let mut deps = BTreeSet::new();
        for word in self.wildcard_depends.split(',') {
            let word = word.trim();
            if word == "$auto" {
                let bin = self.all_binaries();
                let resolved = bin.par_iter()
                    .filter(|bin| !bin.archive_as_symlink_only())
                    .filter_map(|&p| {
                        let bname = p.path()?;
                        match resolve_with_dpkg(bname, &lib_search_paths) {
                            Ok(bindeps) => Some(bindeps),
                            Err(err) => {
                                listener.warning(format!("{err}\nNo $auto deps for {}", bname.display()));
                                None
                            },
                        }
                    }).collect_vec_list();
                deps.extend(resolved.into_iter().flatten().flatten());
            } else {
                let (dep, arch_spec) = get_architecture_specification(word)?;
                if let Some(spec) = arch_spec {
                    let matches = match_architecture(spec, &self.architecture)
                        .inspect_err(|e| listener.warning(format!("Can't get arch spec for '{word}'\n{e}")));
                    if matches.unwrap_or(true) {
                        deps.insert(dep);
                    }
                } else {
                    deps.insert(dep);
                }
            }
        }
        Ok(itertools::Itertools::join(&mut deps.into_iter(), ", "))
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

    fn extended_description(&self, config: &BuildEnvironment) -> CDResult<Option<Cow<'_, str>>> {
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
    pub fn generate_control(&self, config: &BuildEnvironment) -> CDResult<String> {
        use fmt::Write;

        // Create and return the handle to the control file with write access.
        let mut control = String::with_capacity(1024);

        // Write all of the lines required by the control file.
        writeln!(control, "Package: {}", self.deb_name)?;
        writeln!(control, "Version: {}", self.deb_version)?;
        writeln!(control, "Architecture: {}", self.architecture)?;
        let ma = match self.multiarch {
            Multiarch::None => "",
            Multiarch::Same => "same",
            Multiarch::Foreign => "foreign",
        };
        if !ma.is_empty() {
            writeln!(control, "Multi-Arch: {ma}")?;
        }
        if self.is_split_dbgsym_package {
            writeln!(control, "Auto-Built-Package: debug-symbols")?;
        }
        if let Some(homepage) = self.homepage.as_deref().or(self.documentation.as_deref()).or(self.repository.as_deref()) {
            writeln!(control, "Homepage: {homepage}")?;
        }
        if let Some(ref section) = self.section {
            writeln!(control, "Section: {section}")?;
        }
        writeln!(control, "Priority: {}", self.priority)?;
        if let Some(maintainer) = self.maintainer.as_deref() {
            writeln!(control, "Maintainer: {maintainer}")?;
        }

        let installed_size = self.assets.resolved
            .iter()
            .map(|m| (m.source.file_size().unwrap_or(0) + 2047) / 1024) // assume 1KB of fs overhead per file
            .sum::<u64>();

        writeln!(control, "Installed-Size: {installed_size}")?;

        if let Some(deps) = &self.resolved_depends {
            writeln!(control, "Depends: {deps}")?;
        }

        if let Some(ref pre_depends) = self.pre_depends {
            let pre_depends_normalized = pre_depends.trim();

            if !pre_depends_normalized.is_empty() {
                writeln!(control, "Pre-Depends: {pre_depends_normalized}")?;
            }
        }

        if let Some(ref recommends) = self.recommends {
            let recommends_normalized = recommends.trim();

            if !recommends_normalized.is_empty() {
                writeln!(control, "Recommends: {recommends_normalized}")?;
            }
        }

        if let Some(ref suggests) = self.suggests {
            let suggests_normalized = suggests.trim();

            if !suggests_normalized.is_empty() {
                writeln!(control, "Suggests: {suggests_normalized}")?;
            }
        }

        if let Some(ref enhances) = self.enhances {
            let enhances_normalized = enhances.trim();

            if !enhances_normalized.is_empty() {
                writeln!(control, "Enhances: {enhances_normalized}")?;
            }
        }

        if let Some(ref conflicts) = self.conflicts {
            writeln!(control, "Conflicts: {conflicts}")?;
        }
        if let Some(ref breaks) = self.breaks {
            writeln!(control, "Breaks: {breaks}")?;
        }
        if let Some(ref replaces) = self.replaces {
            writeln!(control, "Replaces: {replaces}")?;
        }
        if let Some(ref provides) = self.provides {
            writeln!(control, "Provides: {provides}")?;
        }

        write!(&mut control, "Description:")?;
        for line in self.description.split_by_chars(79) {
            writeln!(control, " {line}")?;
        }

        if let Some(desc) = self.extended_description(config)? {
            for line in desc.split_by_chars(79) {
                writeln!(control, " {line}")?;
            }
        }
        control.push('\n');

        Ok(control)
    }

    pub(crate) fn write_copyright_metadata(&self, has_full_text: bool) -> Result<(String, bool), fmt::Error> {
        let mut copyright = String::new();
        let mut incomplete = false;
        use std::fmt::Write;

        writeln!(copyright, "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/")?;
        writeln!(copyright, "Upstream-Name: {}", self.cargo_crate_name)?;
        if let Some(source) = self.repository.as_deref().or(self.homepage.as_deref()) {
            writeln!(copyright, "Source: {source}")?;
        }
        if let Some(c) = self.copyright.as_deref() {
            writeln!(copyright, "Copyright: {c}")?;
        } else if let Some(m) = self.maintainer.as_deref() {
            writeln!(copyright, "Comment: Copyright information missing (maintainer: {m})")?;
        } else if let Some(l) = self.license_identifier.as_deref().filter(|l| license_doesnt_need_author_info(l)) {
            log::debug!("assuming the license {l} doesn't require copyright owner info");
        } else {
            incomplete = true;
        }
        if let Some(license) = self.license_identifier.as_deref().or(has_full_text.then_some("")) {
            writeln!(copyright, "License: {license}")?;
        }
        Ok((copyright, incomplete))
    }

    pub(crate) fn conf_files(&self) -> Option<String> {
        if self.conf_files.is_empty() {
            return None;
        }
        Some(format_conffiles(&self.conf_files))
    }

    pub(crate) fn split_dbgsym(&mut self) -> Result<Option<Self>, CargoDebError> {
        debug_assert!(self.assets.unresolved.is_empty());
        let (debug_assets, regular): (Vec<_>, Vec<_>) = self.assets.resolved.drain(..).partition(|asset| {
            asset.c.asset_kind == AssetKind::SeparateDebugSymbols
        });
        self.assets.resolved = regular;
        if debug_assets.is_empty() {
            return Ok(None);
        }

        Ok(Some(Self {
            cargo_crate_name: self.cargo_crate_name.clone(),
            deb_name: format!("{}-dbgsym", self.deb_name),
            deb_version: self.deb_version.clone(),
            license_identifier: self.license_identifier.clone(),
            license_file_rel_path: None,
            license_file_skip_lines: 0,
            copyright: None,
            changelog: None,
            homepage: self.homepage.clone(),
            documentation: self.documentation.clone(),
            repository: self.repository.clone(),
            description: format!("Debug symbols for {} v{} ({})", self.deb_name, self.deb_version, self.architecture),
            extended_description: ExtendedDescription::None,
            maintainer: self.maintainer.clone(),
            wildcard_depends: String::new(),
            resolved_depends: Some(format!("{} (= {})", self.deb_name, self.deb_version)),
            pre_depends: None,
            recommends: None,
            suggests: None,
            enhances: None,
            section: Some("debug".into()),
            priority: "extra".into(),
            conflicts: None,
            breaks: None,
            replaces: None,
            provides: None,
            architecture: self.architecture.clone(),
            multiarch: if self.multiarch != Multiarch::None { Multiarch::Same } else { Multiarch::None },
            conf_files: Vec::new(),
            assets: Assets::new(Vec::new(), debug_assets),
            readme_rel_path: None,
            triggers_file_rel_path: None,
            maintainer_scripts_rel_path: None,
            preserve_symlinks: self.preserve_symlinks,
            systemd_units: None,
            default_timestamp: self.default_timestamp,
            is_split_dbgsym_package: true,
        }))
    }
}

fn license_doesnt_need_author_info(license_identifier: &str) -> bool {
     ["UNLICENSED", "PROPRIETARY", "CC-PDDC", "CC0-1.0"].iter().any(|l| l.eq_ignore_ascii_case(license_identifier))
}

const EXPECTED: &str = "Expected items in `assets` to be either `[source, dest, mode]` array, or `{source, dest, mode}` object, or `\"$auto\"`";

impl TryFrom<CargoDebAssetArrayOrTable> for RawAssetOrAuto {
    type Error = String;

    fn try_from(toml: CargoDebAssetArrayOrTable) -> Result<Self, Self::Error> {
        fn parse_chmod(mode: &str) -> Result<u32, String> {
            u32::from_str_radix(mode, 8).map_err(|e| format!("Unable to parse mode argument (third array element) as an octal number in an asset: {e}"))
        }
        let raw_asset = match toml {
            CargoDebAssetArrayOrTable::Table(a) => Self::RawAsset(RawAsset {
                source_path: a.source.into(),
                target_path: a.dest.into(),
                chmod: parse_chmod(&a.mode)?,
            }),
            CargoDebAssetArrayOrTable::Array(a) => {
                let mut a = a.into_iter();
                Self::RawAsset(RawAsset {
                    source_path: PathBuf::from(a.next().ok_or("Missing source path (first array element) in an asset in Cargo.toml")?),
                    target_path: PathBuf::from(a.next().ok_or("missing dest path (second array entry) for asset in Cargo.toml. Use something like \"usr/local/bin/\".")?),
                    chmod: parse_chmod(&a.next().ok_or("Missing mode (third array element) in an asset")?)?
                })
            },
            CargoDebAssetArrayOrTable::Auto(s) if s == "$auto" => Self::Auto,
            CargoDebAssetArrayOrTable::Auto(bad) => {
                return Err(format!("{EXPECTED}, but found a string: '{bad}'"));
            },
            CargoDebAssetArrayOrTable::Invalid(bad) => {
                return Err(format!("{EXPECTED}, but found {}: {bad}", bad.type_str()));
            },
        };
        if let Self::RawAsset(a) = &raw_asset {
            if let Some(msg) = is_trying_to_customize_target_path(&a.source_path) {
                return Err(format!("Please only use `target/release` path prefix for built products, not `{}`.
    {msg}
    The `target/release` is treated as a special prefix, and will be replaced dynamically by cargo-deb with the actual target directory path used by the build.
    ", a.source_path.display()));
            }
        }
        Ok(raw_asset)
    }
}

fn is_trying_to_customize_target_path(p: &Path) -> Option<&'static str> {
    let mut p = p.components().skip_while(|p| matches!(p, Component::ParentDir | Component::CurDir));
    if p.next() != Some(Component::Normal("target".as_ref())) {
        return None;
    }
    let Some(Component::Normal(subdir)) = p.next() else {
        return None;
    };
    if subdir == "debug" {
        return Some("Packaging of development-only binaries is intentionally unsupported in cargo-deb.\n\
            To add debug information or additional assertions use `[profile.release]` in Cargo.toml instead.");
    }
    if subdir.to_str().unwrap_or_default().contains('-')
            && p.next() == Some(Component::Normal("release".as_ref())) {
        return Some("Hardcoding of cross-compilation paths in the configuration is unnecessary, and counter-productive. cargo-deb understands cross-compilation natively and adjusts the path when you use --target.");
    }
    None
}

fn parse_license_file(package: &cargo_toml::Package<CargoPackageMetadata>, license_file: Option<&LicenseFile>) -> CDResult<(Option<PathBuf>, usize)> {
    Ok(match license_file {
        Some(LicenseFile::Vec(args)) => {
            let mut args = args.iter();
            let file = args.next().map(PathBuf::from);
            let lines = args.next().map(|n| n.parse().map_err(|e| CargoDebError::NumParse("invalid number of lines", e))).transpose()?.unwrap_or(0);
            (file, lines)
        },
        Some(LicenseFile::String(s)) => (Some(s.into()), 0),
        None => (package.license_file().map(PathBuf::from), 0),
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

impl BuildEnvironment {
    fn explicit_assets(&self, package_deb: &PackageConfig, assets: Vec<RawAssetOrAuto>, listener: &dyn Listener) -> CDResult<Assets> {
        let custom_profile_target_dir = self.build_profile.profile_name.as_deref().map(|profile| format!("target/{profile}"));

        let mut has_auto = false;

        // Treat all explicit assets as unresolved until after the build step
        let unresolved_assets = assets.into_iter().filter_map(|asset_or_auto| {
            match asset_or_auto {
                RawAssetOrAuto::Auto => {
                    has_auto = true;
                    None
                },
                RawAssetOrAuto::RawAsset(asset) => Some(asset),
            }
        }).map(|RawAsset { source_path, mut target_path, chmod }| {
            // target/release is treated as a magic prefix that resolves to any profile
            let target_artifact_rel_path = source_path.strip_prefix("target/release").ok()
                .or_else(|| source_path.strip_prefix(custom_profile_target_dir.as_ref()?).ok());
            let (is_built, source_path, is_example) = if let Some(rel_path) = target_artifact_rel_path {
                let is_example = rel_path.starts_with("examples");
                (self.find_is_built_file_in_package(rel_path, if is_example { "example" } else { "bin" }), self.path_in_build(rel_path), is_example)
            } else {
                if source_path.to_str().is_some_and(|s| s.starts_with(['/','.']) && s.contains("/target/")) {
                    listener.warning(format!("Only source paths starting with exactly 'target/release/' are detected as Cargo target dir. '{}' does not match the pattern, and will not be built", source_path.display()));
                }
                (IsBuilt::No, self.path_in_package(&source_path), false)
            };

            if package_deb.multiarch != Multiarch::None {
                if let Ok(lib_file_name) = target_path.strip_prefix("usr/lib") {
                    let lib_dir = package_deb.library_install_dir(self.rust_target_triple());
                    if !target_path.starts_with(&lib_dir) {
                        let new_path = lib_dir.join(lib_file_name);
                        log::debug!("multiarch: changed {} to {}", target_path.display(), new_path.display());
                        target_path = new_path;
                    }
                }
            }
            UnresolvedAsset::new(source_path, target_path, chmod, is_built, if is_example { AssetKind::CargoExampleBinary } else { AssetKind::Any })
        }).collect::<Vec<_>>();
        let resolved = if has_auto { self.implicit_assets(package_deb)? } else { vec![] };
        Ok(Assets::new(unresolved_assets, resolved))
    }

    fn implicit_assets(&self, package_deb: &PackageConfig) -> CDResult<Vec<Asset>> {
        let mut implied_assets: Vec<_> = self.build_targets.iter()
            .filter_map(|t| {
                if t.crate_types.iter().any(|ty| ty == "bin") && t.kind.iter().any(|k| k == "bin") {
                    Some(Asset::new(
                        AssetSource::Path(self.path_in_build(&t.name)),
                        Path::new("usr/bin").join(&t.name),
                        0o755,
                        self.is_built_file_in_package(t),
                        AssetKind::Any,
                    ).processed("$auto", t.src_path.clone()))
                } else if t.crate_types.iter().any(|ty| ty == "cdylib") && t.kind.iter().any(|k| k == "cdylib") {
                    let (prefix, suffix) = if self.rust_target_triple.is_none() { (DLL_PREFIX, DLL_SUFFIX) } else { ("lib", ".so") };
                    let lib_name = format!("{prefix}{}{suffix}", t.name);
                    let lib_dir = package_deb.library_install_dir(self.rust_target_triple());
                    Some(Asset::new(
                        AssetSource::Path(self.path_in_build(&lib_name)),
                        lib_dir.join(lib_name),
                        0o644,
                        self.is_built_file_in_package(t),
                        AssetKind::Any,
                    ).processed("$auto", t.src_path.clone()))
                } else {
                    None
                }
            })
            .collect();
        if implied_assets.is_empty() {
            return Err(CargoDebError::BinariesNotFound(package_deb.cargo_crate_name.clone()));
        }
        if let Some(readme_rel_path) = package_deb.readme_rel_path.as_deref() {
            let path = self.path_in_package(readme_rel_path);
            let target_path = Path::new("usr/share/doc")
                .join(&package_deb.deb_name)
                .join(path.file_name().ok_or("bad README path")?);
            implied_assets.push(Asset::new(AssetSource::Path(path), target_path, 0o644, IsBuilt::No, AssetKind::Any)
                .processed("$auto", readme_rel_path.to_path_buf()));
        }
        Ok(implied_assets)
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
