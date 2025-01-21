#![recursion_limit = "128"]
#![allow(clippy::case_sensitive_file_extension_comparisons)]
#![allow(clippy::if_not_else)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::similar_names)]
#![allow(clippy::assigning_clones)] // buggy

/*!

## Making deb packages

If you only want to make some `*.deb` files, and you're not a developer of tools
for Debian packaging, **[see `cargo deb` command usage described in the
README instead](https://github.com/kornelski/cargo-deb#readme)**.

```sh
cargo install cargo-deb
cargo deb # run this in your Cargo project directory
```

## Making tools for making deb packages

The library interface is experimental. See `main.rs` for usage.
*/

pub mod deb {
    pub mod ar;
    pub mod control;
    pub mod tar;
}
#[macro_use]
mod util;
mod dh {
    pub(crate) mod dh_installsystemd;
    pub(crate) mod dh_lib;
}
pub mod listener;
pub(crate) mod parse {
    pub(crate) mod cargo;
    pub(crate) mod manifest;
}
pub use crate::config::{Config, DebugSymbols, PackageConfig};
pub use crate::deb::ar::DebArchive;
pub use crate::error::*;
pub use crate::util::compress;
use crate::util::compress::{CompressConfig, Format};

pub mod assets;
pub mod config;
mod debuginfo;
mod dependencies;
mod error;
pub use debuginfo::strip_binaries;

use crate::assets::compress_assets;
use crate::deb::control::ControlArchiveBuilder;
use crate::deb::tar::Tarball;
use crate::listener::Listener;
use config::{DebConfigOverrides, Multiarch};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};
use itertools::Itertools;

const TAR_REJECTS_CUR_DIR: bool = true;

/// Set by `build.rs`
const DEFAULT_TARGET: &str = env!("CARGO_DEB_DEFAULT_TARGET");

pub struct CargoDeb {
    options: CargoDebOptions,
}

impl CargoDeb {
    #[must_use]
    pub const fn new(options: CargoDebOptions) -> Self {
        Self { options }
    }

    pub fn process(mut self, listener: &dyn Listener) -> CDResult<()> {
        if self.options.install || self.options.target.is_none() {
            warn_if_not_linux(listener); // compiling natively for non-linux = nope
        }

        if self.options.system_xz {
            listener.warning("--system-xz is deprecated, use --compress-system instead.".into());

            self.options.compress_type = Format::Xz;
            self.options.compress_system = true;
        }

        // The profile is selected based on the given ClI options and then passed to
        // cargo build accordingly. you could argue that the other way around is
        // more desirable. However for now we want all commands coming in via the
        // same `interface`
        let selected_profile = self.options.profile;
        if selected_profile.as_deref() == Some("dev") {
            listener.warning("dev profile is not supported and will be a hard error in the future. \
                cargo-deb is for making releases, and it doesn't make sense to use it with dev profiles.".into());
            listener.warning("To enable debug symbols set `[profile.release] debug = true` instead.".into());
        }

        let root_manifest_path = self.options.manifest_path.as_deref().map(Path::new);
        let (mut config, mut package_deb) = Config::from_manifest(
            root_manifest_path,
            self.options.selected_package_name.as_deref(),
            self.options.output_path,
            self.options.target.as_deref(),
            self.options.variant.as_deref(),
            self.options.overrides,
            selected_profile,
            self.options.separate_debug_symbols,
            self.options.compress_debug_symbols,
            self.options.cargo_locking_flags,
            listener,
        )?;
        package_deb.set_multiarch(self.options.multiarch);
        config.prepare_assets_before_build(&mut package_deb, listener)?;

        if !self.options.no_build {
            config.set_cargo_build_flags_for_package(&package_deb, &mut self.options.cargo_build_flags);
            cargo_build(&config, self.options.target.as_deref(), &self.options.cargo_build_cmd, &self.options.cargo_build_flags, self.options.verbose, listener)?;
        }

        package_deb.resolve_assets()?;

        // When cross-compiling, resolve dependencies using libs for the target platform (where multiarch is supported)
        let lib_search_paths = config.rust_target_triple.as_deref().map(|triple| package_deb.multiarch_lib_dirs(triple));
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

        package_deb.resolve_binary_dependencies(&lib_search_paths, listener)?;

        compress_assets(&mut package_deb, listener)?;

        if self.options.strip_override.unwrap_or(config.debug_symbols != DebugSymbols::Keep) {
            strip_binaries(&mut config, &mut package_deb, self.options.target.as_deref(), listener)?;
        } else {
            log::debug!("not stripping debug={:?} strip-flag={:?}", config.debug_symbols, self.options.strip_override);
        }

        package_deb.sort_assets_by_type();

        let generated = write_deb(&config, &package_deb, &CompressConfig {
            fast: self.options.fast,
            compress_type: self.options.compress_type,
            compress_system: self.options.compress_system,
            rsyncable: self.options.rsyncable,
        }, listener)?;

        listener.generated_archive(&generated);

        if self.options.install {
            install_deb(&generated)?;
        }
        Ok(())
    }
}

pub struct CargoDebOptions {
    pub no_build: bool,
    pub strip_override: Option<bool>,
    pub separate_debug_symbols: Option<bool>,
    pub compress_debug_symbols: Option<bool>,
    /// Don't compress heavily
    pub fast: bool,
    /// Build with --verbose
    pub verbose: bool,
    /// Run dpkg -i
    pub install: bool,
    pub selected_package_name: Option<String>,
    pub output_path: Option<String>,
    pub variant: Option<String>,
    pub target: Option<String>,
    pub manifest_path: Option<String>,
    pub cargo_build_cmd: String,
    pub cargo_build_flags: Vec<String>,
    pub overrides: DebConfigOverrides,
    pub compress_type: Format,
    pub compress_system: bool,
    pub system_xz: bool,
    pub rsyncable: bool,
    pub profile: Option<String>,
    pub cargo_locking_flags: CargoLockingFlags,
    /// Use Debian's multiarch lib dirs
    pub multiarch: Multiarch,
}

#[derive(Copy, Clone, Default, Debug)]
pub struct CargoLockingFlags {
    /// `--offline`
    pub offline: bool,
    /// `--frozen`
    pub frozen: bool,
    /// `--locked`
    pub locked: bool,
}

impl CargoLockingFlags {
    #[inline]
    pub(crate) fn flags(self) -> impl Iterator<Item = &'static str> {
        [
            self.offline.then_some("--offline"),
            self.frozen.then_some("--frozen"),
            self.locked.then_some("--locked"),
        ].into_iter().flatten()
    }
}

impl Default for CargoDebOptions {
    fn default() -> Self {
        Self {
            no_build: false,
            strip_override: None,
            separate_debug_symbols: None,
            compress_debug_symbols: None,
            fast: false,
            verbose: false,
            install: false,
            selected_package_name: None,
            output_path: None,
            variant: None,
            target: None,
            manifest_path: None,
            cargo_build_cmd: "build".into(),
            cargo_build_flags: Vec::new(),
            overrides: DebConfigOverrides::default(),
            compress_type: Format::Xz,
            compress_system: false,
            system_xz: false,
            rsyncable: false,
            profile: None,
            cargo_locking_flags: CargoLockingFlags::default(),
            multiarch: Multiarch::None,
        }
    }
}

/// Run `dpkg` to install `deb` archive at the given path
pub fn install_deb(path: &Path) -> CDResult<()> {
    let status = Command::new("sudo").arg("dpkg").arg("-i").arg(path)
        .status()?;
    if !status.success() {
        return Err(CargoDebError::InstallFailed);
    }
    Ok(())
}

pub fn write_deb(config: &Config, package_deb: &PackageConfig, &compress::CompressConfig { fast, compress_type, compress_system, rsyncable }: &compress::CompressConfig, listener: &dyn Listener) -> Result<PathBuf, CargoDebError> {
    let (control_builder, data_result) = rayon::join(
        move || {
            // The control archive is the metadata for the package manager
            let mut control_builder = ControlArchiveBuilder::new(util::compress::select_compressor(fast, compress_type, compress_system)?, package_deb.default_timestamp, listener);
            control_builder.generate_archive(config, package_deb)?;
            Ok::<_, CargoDebError>(control_builder)
        },
        move || {
            // Initialize the contents of the data archive (files that go into the filesystem).
            let dest = util::compress::select_compressor(fast, compress_type, compress_system)?;
            let archive = Tarball::new(dest, package_deb.default_timestamp);
            let compressed = archive.archive_files(package_deb, rsyncable, listener)?;
            let original_data_size = compressed.uncompressed_size;
            Ok::<_, CargoDebError>((compressed.finish()?, original_data_size))
        },
    );
    let control_builder = control_builder?;
    let (data_compressed, original_data_size) = data_result?;
    let control_compressed = control_builder.finish()?.finish()?;

    let mut deb_contents = DebArchive::new(config.deb_output_path(package_deb), package_deb.default_timestamp)?;

    deb_contents.add_control(control_compressed)?;
    let compressed_data_size = data_compressed.len();
    listener.info(format!(
        "compressed/original ratio {compressed_data_size}/{original_data_size} ({}%)",
        compressed_data_size * 100 / original_data_size
    ));
    deb_contents.add_data(data_compressed)?;
    let generated = deb_contents.finish()?;

    let deb_temp_dir = config.deb_temp_dir(package_deb);
    let _ = fs::remove_dir(deb_temp_dir);

    Ok(generated)
}

/// Builds a binary with `cargo build`
pub fn cargo_build(config: &Config, rust_target_triple: Option<&str>, build_command: &str, build_flags: &[String], verbose: bool, listener: &dyn Listener) -> CDResult<()> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&config.cargo_run_current_dir);
    cmd.args(build_command.split(' ')
        .filter(|cmd| if !cmd.starts_with('-') { true } else {
            log::error!("unexpected flag in build command name: {cmd}");
            false
        }));

    cmd.args(build_flags);

    if let Some(rust_target_triple) = rust_target_triple {
        cmd.args(["--target", rust_target_triple]);
        // Set helpful defaults for cross-compiling
        if env::var_os("PKG_CONFIG_ALLOW_CROSS").is_none() && env::var_os("PKG_CONFIG_PATH").is_none() {
            let pkg_config_path = format!("/usr/lib/{}/pkgconfig", debian_triple_from_rust_triple(rust_target_triple));
            if Path::new(&pkg_config_path).exists() {
                cmd.env("PKG_CONFIG_ALLOW_CROSS", "1");
                cmd.env("PKG_CONFIG_PATH", pkg_config_path);
            }
        }
    }

    if config.all_features {
        cmd.arg("--all-features");
    } else {
        if !config.default_features {
            cmd.arg("--no-default-features");
        }
        if !config.features.is_empty() {
            cmd.args(["--features", &config.features.join(",")]);
        }
    }

    if verbose {
        cmd.arg("--verbose");
        listener.info(format!("cargo {}", cmd.get_args().map(|arg| {
            let arg = arg.to_string_lossy();
            if arg.as_bytes().iter().any(|b| b.is_ascii_whitespace()) {
                format!("'{}'", arg.escape_default()).into()
            } else {
                arg
            }
        }).join(" ")));
    } else {
        log::debug!("cargo {:?}", cmd.get_args());
    }

    let status = cmd.status()
        .map_err(|e| CargoDebError::CommandFailed(e, "cargo"))?;
    if !status.success() {
        return Err(CargoDebError::BuildFailed);
    }
    Ok(())
}

// Maps Rust's blah-unknown-linux-blah to Debian's blah-linux-blah. This is debian's multiarch.
fn debian_triple_from_rust_triple(rust_target_triple: &str) -> String {
    let mut p = rust_target_triple.split('-');
    let arch = p.next().unwrap();
    let abi = p.last().unwrap_or("gnu");

    let (darch, dabi) = match (arch, abi) {
        ("i586" | "i686", _) => ("i386", "gnu"),
        ("x86_64", _) => ("x86_64", "gnu"),
        ("aarch64", _) => ("aarch64", "gnu"),
        (arm, abi) if arm.starts_with("arm") || arm.starts_with("thumb") => {
            ("arm", if abi.ends_with("hf") {"gnueabihf"} else {"gnueabi"})
        },
        ("mipsel", _) => ("mipsel", "gnu"),
        (mips @ ("mips64" | "mips64el"), "musl" | "muslabi64") => (mips, "gnuabi64"),
        ("loongarch64", _) => ("loongarch64", "gnu"), // architecture is loong64, tuple is loongarch64!
        (risc, _) if risc.starts_with("riscv64") => ("riscv64", "gnu"),
        (arch, "muslspe") => (arch, "gnuspe"),
        (arch, "musl" | "uclibc") => (arch, "gnu"),
        (arch, abi) => (arch, abi),
    };
    format!("{darch}-linux-{dabi}")
}

/// Debianizes the architecture name. Weirdly, architecture and multiarch use different naming conventions in Debian!
pub(crate) fn debian_architecture_from_rust_triple(rust_target_triple: &str) -> &str {
    let mut parts = rust_target_triple.split('-');
    let arch = parts.next().unwrap();
    let abi = parts.last().unwrap_or("");
    match (arch, abi) {
        // https://wiki.debian.org/Multiarch/Tuples
        // rustc --print target-list
        // https://doc.rust-lang.org/std/env/consts/constant.ARCH.html
        ("aarch64" | "aarch64_be", _) => "arm64",
        ("mips64", "gnuabi32") => "mipsn32",
        ("mips64el", "gnuabi32") => "mipsn32el",
        ("mipsisa32r6", _) => "mipsr6",
        ("mipsisa32r6el", _) => "mipsr6el",
        ("mipsisa64r6", "gnuabi64") => "mips64r6",
        ("mipsisa64r6", "gnuabi32") => "mipsn32r6",
        ("mipsisa64r6el", "gnuabi64") => "mips64r6el",
        ("mipsisa64r6el", "gnuabi32") => "mipsn32r6el",
        ("powerpc", "gnuspe" | "muslspe") => "powerpcspe",
        ("powerpc64", _) => "ppc64",
        ("powerpc64le", _) => "ppc64el",
        ("riscv64gc", _) => "riscv64",
        ("i586" | "i686" | "x86", _) => "i386",
        ("x86_64", "gnux32") => "x32",
        ("x86_64", _) => "amd64",
        ("loongarch64", _) => "loong64",
        (arm, gnueabi) if arm.starts_with("arm") && gnueabi.ends_with("hf") => "armhf",
        (arm, _) if arm.starts_with("arm") || arm.starts_with("thumb") => "armel",
        (other_arch, _) => other_arch,
    }
}

#[test]
fn ensure_all_rust_targets_map_to_debian_targets() {
    assert_eq!(debian_triple_from_rust_triple("armv7-unknown-linux-gnueabihf"), "arm-linux-gnueabihf");

    const DEB_ARCHS: &[&str] = &["alpha", "amd64", "arc", "arm", "arm64", "arm64ilp32", "armel",
    "armhf", "hppa", "hurd-i386", "hurd-amd64", "i386", "ia64", "kfreebsd-amd64",
    "kfreebsd-i386", "loong64", "m68k", "mips", "mipsel", "mips64", "mips64el",
    "mipsn32", "mipsn32el", "mipsr6", "mipsr6el", "mips64r6", "mips64r6el", "mipsn32r6",
    "mipsn32r6el", "powerpc", "powerpcspe", "ppc64", "ppc64el", "riscv64", "s390",
    "s390x", "sh4", "sparc", "sparc64", "uefi-amd6437", "uefi-arm6437", "uefi-armhf37",
    "uefi-i38637", "x32"];

    const DEB_TUPLES: &[&str] = &["aarch64-linux-gnu", "aarch64-linux-gnu_ilp32", "aarch64-uefi",
    "aarch64_be-linux-gnu", "aarch64_be-linux-gnu_ilp32", "alpha-linux-gnu", "arc-linux-gnu",
    "arm-linux-gnu", "arm-linux-gnueabi", "arm-linux-gnueabihf", "arm-uefi", "armeb-linux-gnueabi",
    "armeb-linux-gnueabihf", "hppa-linux-gnu", "i386-gnu", "i386-kfreebsd-gnu",
    "i386-linux-gnu", "i386-uefi", "ia64-linux-gnu", "loongarch64-linux-gnu",
    "m68k-linux-gnu", "mips-linux-gnu", "mips64-linux-gnuabi64", "mips64-linux-gnuabin32",
    "mips64el-linux-gnuabi64", "mips64el-linux-gnuabin32", "mipsel-linux-gnu",
    "mipsisa32r6-linux-gnu", "mipsisa32r6el-linux-gnu", "mipsisa64r6-linux-gnuabi64",
    "mipsisa64r6-linux-gnuabin32", "mipsisa64r6el-linux-gnuabi64", "mipsisa64r6el-linux-gnuabin32",
    "powerpc-linux-gnu", "powerpc-linux-gnuspe", "powerpc64-linux-gnu", "powerpc64le-linux-gnu",
    "riscv64-linux-gnu", "s390-linux-gnu", "s390x-linux-gnu", "sh4-linux-gnu",
    "sparc-linux-gnu", "sparc64-linux-gnu", "x86_64-gnu", "x86_64-kfreebsd-gnu",
    "x86_64-linux-gnu", "x86_64-linux-gnux32", "x86_64-uefi"];

    let list = std::process::Command::new("rustc").arg("--print=target-list").output().unwrap().stdout;
    for rust_target in std::str::from_utf8(&list).unwrap().lines().filter(|a| a.contains("linux")) {
        if ["csky", "hexagon", "riscv32gc"].contains(&rust_target.split_once('-').unwrap().0) {
            continue; // Rust supports more than Debian!
        }
        let deb_arch = debian_architecture_from_rust_triple(rust_target);
        assert!(DEB_ARCHS.contains(&deb_arch), "{rust_target} => {deb_arch}");
        let deb_tuple = debian_triple_from_rust_triple(rust_target);
        assert!(DEB_TUPLES.contains(&deb_tuple.as_str()), "{rust_target} => {deb_tuple}");
    }
}

#[cfg(target_os = "linux")]
fn warn_if_not_linux(_: &dyn Listener) {
}

#[cfg(not(target_os = "linux"))]
fn warn_if_not_linux(listener: &dyn Listener) {
    listener.warning(format!("You're creating a package only for {}, and not for Linux.\nUse --target if you want to cross-compile.", std::env::consts::OS));
}
