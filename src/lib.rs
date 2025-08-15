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
pub use crate::config::{BuildEnvironment, BuildProfile, DebugSymbols, PackageConfig};
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

use crate::assets::{apply_compressed_assets, compressed_assets};
use crate::deb::control::ControlArchiveBuilder;
use crate::deb::tar::Tarball;
use crate::listener::{Listener, PrefixedListener};
use config::BuildOptions;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

const TAR_REJECTS_CUR_DIR: bool = true;

/// Set by `build.rs`
const DEFAULT_TARGET: &str = env!("CARGO_DEB_DEFAULT_TARGET");

pub const DBGSYM_DEFAULT: bool = cfg!(feature = "default_enable_dbgsym");
pub const SEPARATE_DEBUG_SYMBOLS_DEFAULT: bool = cfg!(feature = "default_enable_separate_debug_symbols");
pub const COMPRESS_DEBUG_SYMBOLS_DEFAULT: bool = cfg!(feature = "default_enable_compress_debug_symbols");

pub struct CargoDeb<'tmp> {
    pub options: BuildOptions<'tmp>,
    pub no_build: bool,
    /// Build with --verbose
    pub verbose_cargo_build: bool,
    /// More info from cargo deb
    pub verbose: bool,
    pub compress_config: CompressConfig,
    /// User-configured output path for *.deb
    pub deb_output: Option<OutputPath<'tmp>>,
    /// Run dpkg -i; run for dbsym
    pub install: (bool, bool),
}

pub struct OutputPath<'tmp> {
    pub path: &'tmp Path,
    pub is_dir: bool,
}

impl CargoDeb<'_> {
    pub fn process(mut self, listener: &dyn Listener) -> CDResult<()> {
        if self.install.0 || self.options.rust_target_triples.is_empty() {
            warn_if_not_linux(listener); // compiling natively for non-linux = nope
        }

        if self.options.debug.generate_dbgsym_package == Some(true) {
            let _ = self.options.debug.separate_debug_symbols.get_or_insert(true);
        }
        let asked_for_dbgsym_package = self.options.debug.generate_dbgsym_package.unwrap_or(false);
        let single_target_needs_back_compat = self.deb_output.is_none() && self.options.rust_target_triples.len() == 1;

        // The profile is selected based on the given ClI options and then passed to
        // cargo build accordingly. you could argue that the other way around is
        // more desirable. However for now we want all commands coming in via the
        // same `interface`
        if matches!(self.options.build_profile.profile_name(), "debug" | "dev") {
            listener.warning("dev profile is not supported and will be a hard error in the future. \
                cargo-deb is for making releases, and it doesn't make sense to use it with dev profiles.\n\
                To enable debug symbols set `[profile.release] debug = 1` instead, or use --debug-override. \
                Cargo also supports custom profiles, you can make `[profile.dist]`, etc.".into());
        }

        let (config, package_debs) = BuildEnvironment::from_manifest(self.options, listener)?;

        if !self.no_build {
            config.cargo_build(&package_debs, self.verbose, self.verbose_cargo_build, listener)?;
        }

        let common_suffix_len = Self::rust_target_triple_common_suffix_len(&package_debs);

        let tmp_dir;
        let output = if let Some(d) = self.deb_output { d } else {
            tmp_dir = config.default_deb_output_dir();
            OutputPath { path: &tmp_dir, is_dir: true }
        };

        package_debs.into_par_iter().try_for_each(|package_deb| {
            let tmp_prefix;
            let tmp_listener;
            let mut listener = listener;
            if common_suffix_len != 0 {
                let target = package_deb.rust_target_triple.as_deref().unwrap_or(DEFAULT_TARGET);
                let target = target.get(..target.len().saturating_sub(common_suffix_len)).unwrap_or(target);
                tmp_prefix = format!("{target}: ");
                tmp_listener = PrefixedListener(&tmp_prefix, listener);
                listener = &tmp_listener;
            }

            Self::process_package(package_deb, &config, listener, &self.compress_config, &output, self.install, asked_for_dbgsym_package, single_target_needs_back_compat)
        })
    }

    fn process_package(mut package_deb: PackageConfig, config: &BuildEnvironment, listener: &dyn Listener, compress_config: &CompressConfig, output: &OutputPath<'_>, (install, install_dbgsym): (bool, bool), asked_for_dbgsym_package: bool, needs_back_compat: bool) -> CDResult<()> {
        package_deb.resolve_assets(listener)?;

        let (depends, compressed_assets) = rayon::join(
            || package_deb.resolved_binary_dependencies(listener),
            || compressed_assets(&package_deb, listener),
        );

        debug_assert!(package_deb.resolved_depends.is_none());
        package_deb.resolved_depends = Some(depends?);
        apply_compressed_assets(&mut package_deb, compressed_assets?);

        strip_binaries(config, &mut package_deb, asked_for_dbgsym_package, listener)?;

        let generate_dbgsym_package = matches!(config.debug_symbols, DebugSymbols::Separate { generate_dbgsym_package: true, .. });
        let package_dbgsym_ddeb = generate_dbgsym_package.then(|| package_deb.split_dbgsym()).flatten();

        if package_dbgsym_ddeb.is_none() && generate_dbgsym_package {
            listener.warning("No debug symbols found. Skipping dbgsym.ddeb".into());
        }

        let (generated_deb, generated_dbgsym_ddeb) = rayon::join(
            || {
                package_deb.sort_assets_by_type();
                write_deb(
                    config,
                    package_deb.deb_output_path(output),
                    &package_deb,
                    compress_config,
                    listener,
                )
            },
            || package_dbgsym_ddeb.map(|mut ddeb| {
                ddeb.sort_assets_by_type();
                write_deb(
                    config,
                    ddeb.deb_output_path(output),
                    &ddeb,
                    compress_config,
                    &PrefixedListener("ddeb: ", listener),
                )
            }),
        );
        let generated_deb = generated_deb?;
        let generated_dbgsym_ddeb = generated_dbgsym_ddeb.transpose()?;

        if let Some(generated) = &generated_dbgsym_ddeb {
            let _ = back_compat_copy(generated, &package_deb, needs_back_compat);
            listener.generated_archive(generated);
        }
        let _ = back_compat_copy(&generated_deb, &package_deb, needs_back_compat);
        listener.generated_archive(&generated_deb);

        if install {
            if let Some(dbgsym_ddeb) = generated_dbgsym_ddeb.as_deref().filter(|_| install_dbgsym) {
                install_debs(&[&generated_deb, dbgsym_ddeb])?;
            } else {
                install_debs(&[&generated_deb])?;
            }
        }
        Ok(())
    }

    /// given [a-linux-gnu, b-linux gnu] return len to strip for [a, b]
    fn rust_target_triple_common_suffix_len(package_debs: &[PackageConfig]) -> usize {
        if package_debs.len() < 2 {
            return 0;
        }
        let targets = package_debs.iter()
            .map(|p| p.rust_target_triple.as_deref().unwrap_or(DEFAULT_TARGET))
            .collect::<Vec<_>>();
        let Some((&(mut common_suffix), rest)) = targets.split_first() else {
            return 0;
        };

        for &label in rest {
            let common_len = common_suffix.split('-').rev()
                .zip(label.split('-').rev())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a.len() + 1)
                .sum::<usize>();
            common_suffix = &common_suffix[common_suffix.len().saturating_sub(common_len)..];
        }
        common_suffix.len()
    }
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

impl Default for CargoDeb<'_> {
    fn default() -> Self {
        Self {
            options: BuildOptions::default(),
            no_build: false,
            deb_output: None,
            verbose: false,
            verbose_cargo_build: false,
            install: (false, false),
            compress_config: CompressConfig {
                fast: false,
                compress_type: Format::Xz,
                compress_system: false,
                rsyncable: false,
            },
        }
    }
}

/// Run `dpkg` to install `deb` archive at the given path
pub fn install_debs(paths: &[&Path]) -> CDResult<()> {
    let no_sudo = std::env::var_os("EUID").or_else(|| std::env::var_os("UID")).is_some_and(|v| v == "0");
    match install_debs_inner(paths, no_sudo) {
        Err(CargoDebError::CommandFailed(_, cmd)) if cmd == "sudo" => {
            install_debs_inner(paths, true)
        },
        res => res,
    }
}

fn install_debs_inner(paths: &[&Path], no_sudo: bool) -> CDResult<()> {
    let args = ["dpkg", "-i", "--"];
    let (exe, args) = if no_sudo {
        ("dpkg", &args[1..])
    } else {
        ("sudo", &args[..])
    };
    let mut cmd = Command::new(exe);
    cmd.args(args);
    cmd.args(paths);
    log::debug!("{exe} {:?}", cmd.get_args());
    let status = cmd.status()
        .map_err(|e| CargoDebError::CommandFailed(e, exe.into()))?;
    if !status.success() {
        return Err(CargoDebError::InstallFailed(status));
    }
    Ok(())
}

pub fn write_deb(config: &BuildEnvironment, deb_output_path: PathBuf, package_deb: &PackageConfig, &CompressConfig { fast, compress_type, compress_system, rsyncable }: &CompressConfig, listener: &dyn Listener) -> Result<PathBuf, CargoDebError> {
    let (deb_contents, data_result) = rayon::join(
        move || {
            // The control archive is the metadata for the package manager
            let mut control_builder = ControlArchiveBuilder::new(util::compress::select_compressor(fast, compress_type, compress_system)?, package_deb.default_timestamp, listener);
            control_builder.generate_archive(config, package_deb)?;
            let control_compressed = control_builder.finish()?.finish()?;

            let mut deb_contents = DebArchive::new(deb_output_path, package_deb.default_timestamp)?;
            let compressed_control_size = control_compressed.len();
            deb_contents.add_control(control_compressed)?;
            Ok::<_, CargoDebError>((deb_contents, compressed_control_size))
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
    let (mut deb_contents, compressed_control_size) = deb_contents?;
    let (data_compressed, original_data_size) = data_result?;

    let compressed_size = data_compressed.len() + compressed_control_size;
    let original_size = original_data_size + compressed_control_size; // doesn't track control size
    listener.progress("Compressed", format!(
        "{}KB to {}KB (by {}%)",
        original_data_size / 1000,
        compressed_size / 1000,
        (original_size.saturating_sub(compressed_size)) * 100 / original_size,
    ));
    deb_contents.add_data(data_compressed)?;
    let generated = deb_contents.finish()?;

    let deb_temp_dir = config.deb_temp_dir(package_deb);
    let _ = fs::remove_dir(&deb_temp_dir);

    Ok(generated)
}

// Maps Rust's blah-unknown-linux-blah to Debian's blah-linux-blah. This is debian's multiarch.
fn debian_triple_from_rust_triple(rust_target_triple: &str) -> String {
    let mut p = rust_target_triple.split('-');
    let arch = p.next().unwrap();
    let abi = p.next_back().unwrap_or("gnu");

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
    let abi = parts.next_back().unwrap_or("");
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
        if ["csky", "hexagon", "riscv32gc", "wasm32"].contains(&rust_target.split_once('-').unwrap().0) {
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

// TODO: deprecated, remove
#[cold]
fn back_compat_copy(path: &Path, package_deb: &PackageConfig, enable: bool) -> Option<()> {
    if !enable {
        return None;
    }
    let previous_path = path.parent()?.parent()?
        .join(package_deb.rust_target_triple.as_deref()?)
        .join("debian")
        .join(path.file_name()?);
    let _ = fs::create_dir_all(previous_path.parent()?);
    fs::hard_link(path, &previous_path)
        .or_else(|_| fs::copy(path, &previous_path).map(drop))
        .inspect_err(|e| log::warn!("can't copy {} to {}: {e}", path.display(), previous_path.display()))
        .ok()
}
