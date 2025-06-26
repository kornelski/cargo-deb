use anstream::{AutoStream, ColorChoice};
use cargo_deb::compress::{CompressConfig, Format};
use cargo_deb::config::{BuildOptions, CompressDebugSymbols, DebugSymbolOptions, Multiarch};
use cargo_deb::{listener, BuildProfile, CargoDeb, CargoLockingFlags};
use clap::{Arg, ArgAction, Command};
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let matches = Command::new("cargo-deb")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Create Debian packages from Cargo projects\nhttps://lib.rs/cargo-deb")
        .arg(Arg::new("output").short('o').long("output").help("Write .deb to this file or directory [default: target/debian]").num_args(1).value_name("path"))
        .arg(Arg::new("package").short('p').long("package").help("Select which package to use in a Cargo workspace").num_args(1).value_name("name"))
        .arg(Arg::new("manifest-path").long("manifest-path").help("Select package by the path to Cargo.toml project file").num_args(1).value_name("./Cargo.toml"))
        .arg(Arg::new("target").long("target").help("Rust target platform for cross-compilation").num_args(1).value_name("triple"))
        .arg(Arg::new("multiarch").long("multiarch")
            .num_args(1).value_parser(["none", "same", "foreign"])
            .help("Put libs in /usr/lib/$arch-linux-gnu/")
            .long_help("If `same` or `foreign`, puts libs in /usr/lib/$arch-linux-gnu/ to support multiple architectures. `foreign` is for packages that don't run binaries on the host machine.\nSee https://wiki.debian.org/Multiarch/HOWTO")
            .hide_possible_values(true)
            .default_value("none").value_name("same|foreign"))
        .arg(Arg::new("profile").long("profile").help("Select which Cargo build profile to use").num_args(1).value_name("release|<custom>"))
        .arg(Arg::new("install").long("install").action(ArgAction::SetTrue).help("Immediately install the created deb package"))
        .arg(Arg::new("no-install-dbgsym").long("no-install-dbgsym").action(ArgAction::SetTrue).requires("install").requires("dbgsym")
            .hide_short_help(true).help("Immediately install the created deb package, but without dbgsym package"))
        .arg(Arg::new("quiet").short('q').long("quiet").action(ArgAction::SetTrue).help("Don't print warnings"))
        .arg(Arg::new("verbose").short('v').long("verbose").action(ArgAction::Count).conflicts_with("quiet").help("Print progress; -vv for verbose Cargo builds"))
        .arg(Arg::new("color").long("color").action(ArgAction::Set).value_parser(["auto", "always", "never"])
            .hide_short_help(true).help("ANSI formatting of verbose messages"))
        .next_help_heading("Debug info")
        .arg(Arg::new("dbgsym").long("dbgsym").action(ArgAction::SetTrue)
            .hide_short_help(cargo_deb::DBGSYM_DEFAULT).help("Move debug symbols into a separate -dbgsym.ddeb package"))
        .arg(Arg::new("no-dbgsym").long("no-dbgsym").action(ArgAction::SetTrue).conflicts_with("dbgsym")
            .hide_short_help(!cargo_deb::DBGSYM_DEFAULT).help("Don't make a dbgsym.ddeb package"))
        .arg(Arg::new("strip").long("strip").action(ArgAction::SetTrue).help("Always try to strip debug symbols").conflicts_with("dbgsym"))
        .arg(Arg::new("no-strip").long("no-strip").action(ArgAction::SetTrue).conflicts_with_all(["separate-debug-symbols", "dbgsym"])
            .hide_short_help(true).help("Do not run `strip` command if possible"))
        .arg(Arg::new("separate-debug-symbols").long("separate-debug-symbols").action(ArgAction::SetTrue)
            .hide_short_help(cargo_deb::SEPARATE_DEBUG_SYMBOLS_DEFAULT).help("Move debug symbols to a .debug file in the same package"))
        .arg(Arg::new("no-separate-debug-symbols").long("no-separate-debug-symbols").action(ArgAction::SetTrue).conflicts_with_all(["separate-debug-symbols", "dbgsym"])
            .hide_short_help(!cargo_deb::SEPARATE_DEBUG_SYMBOLS_DEFAULT).help("Do not strip debug symbols into a separate .debug file"))
        .arg(Arg::new("compress-debug-symbols").long("compress-debug-symbols").alias("compress-debug-sections").action(ArgAction::Set)
            .require_equals(true).num_args(0..=1).default_missing_value("auto").value_name("zstd|zlib").value_parser(["zstd", "zlib", "auto"])
            .help("Apply `objcopy --compress-debug-sections`").hide_possible_values(true)
            .long_help("Apply `objcopy --compress-debug-sections` when creating separate debug symbols or dbgsym. zlib is compatible with Rust's backtraces, zstd is smaller."))
        .arg(Arg::new("no-compress-debug-symbols").long("no-compress-debug-symbols").action(ArgAction::SetTrue).conflicts_with("compress-debug-symbols")
            .hide_short_help(!cargo_deb::COMPRESS_DEBUG_SYMBOLS_DEFAULT))
        .next_help_heading("Metadata overrides")
        .arg(Arg::new("variant").long("variant").num_args(1).value_name("name").help("Alternative `[package.metadata.deb.variants.*]` config section to use"))
        .arg(Arg::new("deb-version").long("deb-version").num_args(1).value_name("version").help("Override version string of the package (including revision)"))
        .arg(Arg::new("deb-revision").long("deb-revision").num_args(1).value_name("num").conflicts_with("deb-version")
            .help("Override revision suffix string of the package [default: 1]"))
        .arg(Arg::new("maintainer").long("maintainer").num_args(1).value_name("name").help("Override Maintainer field"))
        .arg(Arg::new("section").long("section").num_args(1).value_name("section")
            .hide_short_help(true).help("Set the application category for this package"))
        .next_help_heading("Build overrides")
        .arg(Arg::new("no-build").long("no-build").action(ArgAction::SetTrue)
            .hide_short_help(true).help("Assume the project is already built. Use for complex projects that require non-Cargo build commands"))
        .arg(Arg::new("cargo-build").long("cargo-build").num_args(1).value_name("subcommand").default_value("build").conflicts_with("no-build")
            .hide_short_help(true).help("Override `build` in `cargo build`").hide_default_value(true))
        .arg(Arg::new("override-debug").long("override-debug").num_args(1).value_name("Cargo.toml debug option").value_parser(["none", "line-tables-only", "limited", "full"])
            .hide_short_help(true).help("Override `[profile.release] debug` value using Cargo's env vars"))
        .arg(Arg::new("override-lto").long("override-lto").num_args(1).value_name("Cargo.toml lto option").value_parser(["thin", "fat"])
            .hide_short_help(true).help("Override `[profile.release] lto` value using Cargo's env vars"))
        .next_help_heading("Deb compression")
        .arg(Arg::new("fast").long("fast").action(ArgAction::SetTrue)
            .help("Use faster compression, which makes a larger deb file"))
        .arg(Arg::new("compress-type").short('Z').long("compress-type").num_args(1).value_name("gz|xz").value_parser(["xz", "gz", "gzip"]).default_value("xz")
            .help("Compress with the given compression format").hide_possible_values(true))
        .arg(Arg::new("compress-system").long("compress-system").alias("system-xz").action(ArgAction::SetTrue)
            .help("Use the corresponding command-line tool for compression"))
        .arg(Arg::new("rsyncable").long("rsyncable").action(ArgAction::SetTrue).hide_short_help(true)
            .help("Use worse compression, but reduce differences between versions of packages"))
        .next_help_heading("Cargo")
        .arg(Arg::new("features").short('F').long("features").num_args(1).value_name("list").help("Can also be set in Cargo.toml `[package.metadata.deb]`"))
        .arg(Arg::new("no-default-features").long("no-default-features").action(ArgAction::SetTrue).help("Can also be set in Cargo.toml `[package.metadata.deb]`"))
        .arg(Arg::new("all-features").long("all-features").action(ArgAction::SetTrue).conflicts_with("no-default-features").help("Passed to Cargo"))
        .arg(Arg::new("offline").long("offline").action(ArgAction::SetTrue).help("Use only cached registry and cached packages"))
        .arg(Arg::new("locked").long("locked").action(ArgAction::SetTrue).help("Require Cargo.lock to be up-to-date"))
        .arg(Arg::new("frozen").long("frozen").action(ArgAction::SetTrue).hide_short_help(true).help("Passed to Cargo"))
        .arg(Arg::new("-- other cargo arguments").num_args(0..).help("Free arguments passed to cargo build"))
        .after_help("Use --help to show more options")
        .after_long_help("See https://lib.rs/crates/cargo-deb for more info")
        .get_matches();

    let verbose_count = matches.get_count("verbose");
    {
        let mut logger = env_logger::builder();
        if verbose_count > 3 {
            logger.filter_level(log::LevelFilter::max());
        }
        logger.init();
    }

    let compress_type = match matches.get_one::<String>("compress-type").map(|s| s.as_str()) {
        Some("gz" | "gzip") => Format::Gzip,
        Some("xz") | None => Format::Xz,
        _ => Format::Xz,
    };

    let multiarch = match matches.get_one::<String>("multiarch").map_or("none", |s| s.as_str()) {
        "same" => Multiarch::Same,
        "foreign" => Multiarch::Foreign,
        _ => Multiarch::None,
    };

    // `cargo deb` invocation passes the `deb` arg through.
    let mut free_args: Vec<String> = matches.get_many("-- other cargo arguments").unwrap_or_default().cloned().collect();
    if free_args.first().is_some_and(|arg| arg == "deb") {
        free_args.remove(0);
    }

    let quiet = matches.get_flag("quiet");
    let verbose = verbose_count > 0 || (!quiet && env::var_os("RUST_LOG").is_some_and(|v| v == "debug"));
    let verbose_cargo_build = verbose_count > 1;
    let color = matches.get_one::<String>("color").and_then(|v| match v.as_str() {
        "always" => Some(ColorChoice::Always),
        "never" => Some(ColorChoice::Never),
        _ => None,
    }).unwrap_or_else(|| AutoStream::choice(&std::io::stderr()));

    // Listener conditionally prints warnings
    let listener: &dyn listener::Listener = &listener::StdErrListener {
        verbose, quiet, color,
    };

    let deb_version = matches.get_one::<String>("deb-version").cloned();
    let deb_revision = matches.get_one::<String>("deb-revision").cloned();

    if deb_version.is_some() && deb_revision.as_deref().is_some_and(|r| !r.is_empty()) {
        listener.warning(format!("--deb-version takes precedence over --deb-revision. Revision '{}' will be ignored", deb_revision.as_deref().unwrap_or_default()));
    }

    let install = matches.get_flag("install");

    let compress_debug_symbols = matches.get_one::<String>("compress-debug-symbols").map(|s| match &**s {
        "zlib" => CompressDebugSymbols::Zlib,
        "zstd" => CompressDebugSymbols::Zstd,
        _ => CompressDebugSymbols::Auto,
    }).or_else(|| {
        matches.get_flag("no-compress-debug-symbols").then_some(CompressDebugSymbols::No)
    });

    match (CargoDeb {
        deb_output_path: matches.get_one::<String>("output").cloned(),
        no_build: matches.get_flag("no-build"),
        verbose,
        verbose_cargo_build,
        install,
        install_without_dbgsym: matches.get_flag("no-install-dbgsym"),
        compress_config: CompressConfig {
            // when installing locally it won't be transferred anywhere, so allow faster compression
            fast: install || matches.get_flag("fast"),
            compress_type,
            compress_system: matches.get_flag("compress-system"),
            rsyncable: matches.get_flag("rsyncable"),
        },
        options: BuildOptions {
            config_variant: matches.get_one::<String>("variant").map(|x| x.as_str()),
            rust_target_triple: matches.get_one::<String>("target").cloned().or_else(|| std::env::var("CARGO_BUILD_TARGET").ok()).as_deref(),
            multiarch,
            selected_package_name: matches.get_one::<String>("package").map(|x| x.as_str()),
            manifest_path: matches.get_one::<String>("manifest-path").map(|v| v.as_ref()),
            cargo_build_cmd: matches.get_one::<String>("cargo-build").cloned(),
            cargo_build_flags: free_args,
            debug: DebugSymbolOptions {
                strip_override: matches.get_flag("strip").then_some(true)
                    .or_else(|| matches.get_flag("no-strip").then_some(false)),
                separate_debug_symbols: matches.get_flag("separate-debug-symbols").then_some(true)
                    .or_else(|| matches.get_flag("no-separate-debug-symbols").then_some(false)),
                compress_debug_symbols,
                generate_dbgsym_package: matches.get_flag("dbgsym").then_some(true)
                    .or_else(|| matches.get_flag("no-dbgsym").then_some(false)),
            },
            overrides: {
                let mut tmp = cargo_deb::config::DebConfigOverrides::default();
                tmp.deb_version = deb_version;
                tmp.deb_revision = deb_revision;
                tmp.maintainer = matches.get_one::<String>("maintainer").cloned();
                tmp.section = matches.get_one::<String>("section").cloned();
                tmp.features = matches.get_many::<String>("features").unwrap_or_default().cloned().collect();
                tmp.no_default_features = matches.get_flag("no-default-features");
                tmp.all_features = matches.get_flag("all-features");
                tmp
            },
            build_profile: BuildProfile {
                profile_name: matches.get_one::<String>("profile").cloned(),
                override_debug: matches.get_one::<String>("override-debug").cloned(),
                override_lto: matches.get_one::<String>("override-lto").cloned(),
            },
            cargo_locking_flags: CargoLockingFlags {
                offline: matches.get_flag("offline"),
                frozen: matches.get_flag("frozen"),
                locked: matches.get_flag("locked"),
            },
        },
    }).process(listener) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            listener.error(&err);
            ExitCode::FAILURE
        },
    }
}
