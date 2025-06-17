use cargo_deb::compress::Format;
use cargo_deb::config::Multiarch;
use cargo_deb::{listener, CargoDeb, CargoDebError, CargoDebOptions, CargoLockingFlags};
use clap::{Arg, ArgAction, Command};
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::init();

    let matches = Command::new("cargo-deb")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Create Debian packages from Cargo projects\nhttps://lib.rs/cargo-deb")
        .arg(Arg::new("output").short('o').long("output").help("Write .deb to this file or directory [default: target/debian]").num_args(1).value_name("path"))
        .arg(Arg::new("package").short('p').long("package").help("Select which Cargo workspace package to use").num_args(1).value_name("name"))
        .arg(Arg::new("manifest-path").long("manifest-path").help("Select package by the path to Cargo.toml project file").num_args(1).value_name("./Cargo.toml"))
        .arg(Arg::new("target").long("target").help("Rust target platform for cross-compilation").num_args(1).value_name("triple"))
        .arg(Arg::new("multiarch").long("multiarch").value_parser(["none", "same", "foreign"]).help("Put libs in /usr/lib/$arch-linux-gnu/").num_args(1).default_value("none").value_name("foreign"))
        .arg(Arg::new("profile").long("profile").help("Select which Cargo build profile to use").num_args(1).value_name("release|<custom>"))
        .arg(Arg::new("install").long("install").help("Immediately install the created deb package").action(ArgAction::SetTrue))
        .arg(Arg::new("cargo-build").long("cargo-build").help("Override cargo build subcommand").num_args(1).value_name("subcommand"))
        .arg(Arg::new("no-build").long("no-build").help("Assume the project is already built").action(ArgAction::SetTrue))
        .arg(Arg::new("quiet").short('q').long("quiet").help("Don't print warnings").action(ArgAction::SetTrue))
        .arg(Arg::new("verbose").short('v').long("verbose").help("Print progress").action(ArgAction::SetTrue))
        .next_help_heading("Debug info")
        .arg(Arg::new("strip").long("strip").help("Always try to strip debug symbols").action(ArgAction::SetTrue))
        .arg(Arg::new("no-strip").long("no-strip").help("Do not strip debug symbols from the binary").action(ArgAction::SetTrue))
        .arg(Arg::new("separate-debug-symbols").long("separate-debug-symbols").help("Strip debug symbols into a separate .debug file").action(ArgAction::SetTrue))
        .arg(Arg::new("no-separate-debug-symbols").long("no-separate-debug-symbols").help("Do not strip debug symbols into a separate .debug file").action(ArgAction::SetTrue))
        .arg(Arg::new("compress-debug-symbols").long("compress-debug-symbols").help("Apply `objcopy --compress-debug-sections`").action(ArgAction::SetTrue))
        .next_help_heading("Metadata overrides")
        .arg(Arg::new("variant").long("variant").help("Alternative Cargo.toml configuration section to use").num_args(1).value_name("name"))
        .arg(Arg::new("deb-version").long("deb-version").help("Override version string for the package").num_args(1).value_name("version"))
        .arg(Arg::new("deb-revision").long("deb-revision").help("Override revision suffix string for the package").num_args(1).value_name("num"))
        .arg(Arg::new("maintainer").long("maintainer").help("Override Maintainer field").num_args(1).value_name("name"))
        .arg(Arg::new("section").long("section").help("Set the application category for this package").num_args(1).value_name("section"))
        .next_help_heading("Deb compression")
        .arg(Arg::new("fast").long("fast").help("Use faster compression, which makes a larger deb file").action(ArgAction::SetTrue))
        .arg(Arg::new("compress-type").short('Z').long("compress-type").help("Compress with the given compression format").num_args(1).value_name("gz|xz"))
        .arg(Arg::new("compress-system").long("compress-system").alias("system-xz").help("Use the corresponding command-line tool for compression").action(ArgAction::SetTrue))
        .arg(Arg::new("rsyncable").long("rsyncable").help("Use worse compression, but reduce differences between versions of packages").action(ArgAction::SetTrue))
        .next_help_heading("Cargo")
        .arg(Arg::new("offline").long("offline").help("Passed to Cargo").action(ArgAction::SetTrue))
        .arg(Arg::new("locked").long("locked").help("Passed to Cargo").action(ArgAction::SetTrue))
        .arg(Arg::new("frozen").long("frozen").help("Passed to Cargo").action(ArgAction::SetTrue))
        .arg(Arg::new("features").short('F').long("features").num_args(1).value_name("list").help("Can also be set in Cargo.toml package.metadata.deb"))
        .arg(Arg::new("all-features").long("all-features").help("Passed to Cargo").action(ArgAction::SetTrue))
        .arg(Arg::new("no-default-features").long("no-default-features").help("Can also be set in Cargo.toml package.metadata.deb").action(ArgAction::SetTrue))
        .arg(Arg::new("-- other cargo arguments").help("Free arguments passed to cargo build").num_args(0..))
        .get_matches();

    let install = matches.get_flag("install");

    let compress_type = match matches.get_one::<String>("compress-type").map(|s| s.as_str()) {
        Some("gz" | "gzip") => Format::Gzip,
        Some("xz") | None => Format::Xz,
        _ => {
            print_error(&CargoDebError::Str("unrecognized compression format. Supported: gzip, xz"));
            return ExitCode::FAILURE;
        },
    };

    let multiarch = match matches.get_one::<String>("multiarch").map_or("none", |s| s.as_str()) {
        "none" => Multiarch::None,
        "same" => Multiarch::Same,
        "foreign" => Multiarch::Foreign,
        _ => {
            print_error(&CargoDebError::Str("multiarch must be 'none', 'same', or 'foreign'. https://wiki.debian.org/Multiarch/HOWTO"));
            return ExitCode::FAILURE;
        },
    };

    // `cargo deb` invocation passes the `deb` arg through.
    let mut free_args: Vec<String> = matches.get_many::<String>("-- other cargo arguments").unwrap_or_default().cloned().collect();
    if free_args.first().is_some_and(|arg| arg == "deb") {
        free_args.remove(0);
    }

    let quiet = matches.get_flag("quiet");
    let verbose = matches.get_flag("verbose") || env::var_os("RUST_LOG").is_some_and(|v| v == "debug");

    // Listener conditionally prints warnings
    let (listener_tmp1, listener_tmp2);
    let listener: &dyn listener::Listener = if quiet {
        listener_tmp1 = listener::NoOpListener;
        &listener_tmp1
    } else {
        listener_tmp2 = listener::StdErrListener { verbose };
        &listener_tmp2
    };

    let deb_version = matches.get_one::<String>("deb-version").cloned();
    let deb_revision = matches.get_one::<String>("deb-revision").cloned();

    if deb_version.is_some() && deb_revision.as_deref().is_some_and(|r| !r.is_empty()) {
        listener.warning(format!("--deb-version takes precedence over --deb-revision. Revision '{}' will be ignored", deb_revision.as_deref().unwrap_or_default()));
    }

    match CargoDeb::new(CargoDebOptions {
        no_build: matches.get_flag("no-build"),
        strip_override: if matches.get_flag("strip") { Some(true) } else if matches.get_flag("no-strip") { Some(false) } else { None },
        separate_debug_symbols: if matches.get_flag("separate-debug-symbols") { Some(true) } else if matches.get_flag("no-separate-debug-symbols") { Some(false) } else { None },
        compress_debug_symbols: if matches.get_flag("compress-debug-symbols") { Some(true) } else { None },
        verbose,
        install,
        // when installing locally it won't be transferred anywhere, so allow faster compression
        fast: install || matches.get_flag("fast"),
        variant: matches.get_one::<String>("variant").cloned(),
        target: matches.get_one::<String>("target").cloned().or_else(|| std::env::var("CARGO_BUILD_TARGET").ok()),
        multiarch,
        output_path: matches.get_one::<String>("output").cloned(),
        selected_package_name: matches.get_one::<String>("package").cloned(),
        manifest_path: matches.get_one::<String>("manifest-path").cloned(),
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
        compress_type,
        compress_system: matches.get_flag("compress-system"),
        system_xz: false,
        rsyncable: matches.get_flag("rsyncable"),
        profile: matches.get_one::<String>("profile").cloned(),
        cargo_build_cmd: matches.get_one::<String>("cargo-build").map_or("build", |s| s.as_str()).into(),
        cargo_locking_flags: CargoLockingFlags {
            offline: matches.get_flag("offline"),
            frozen: matches.get_flag("frozen"),
            locked: matches.get_flag("locked"),
        },
        cargo_build_flags: free_args,
    }).process(listener) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            print_error(&err);
            ExitCode::FAILURE
        },
    }
}

#[allow(deprecated)]
fn err_cause(err: &dyn std::error::Error, max: usize) {
    if let Some(reason) = err.cause() {
        eprintln!("  because: {reason}");
        if max > 0 {
            err_cause(reason, max - 1);
        }
    }
}

fn print_error(err: &dyn std::error::Error) {
    eprintln!("cargo-deb: {err}");
    err_cause(err, 3);
}
