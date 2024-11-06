use cargo_deb::compress::Format;
use cargo_deb::{listener, CargoDeb, CargoDebError, CargoDebOptions, CargoLockingFlags};
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::init();

    let args: Vec<String> = env::args().collect();

    let mut cli_opts = getopts::Options::new();
    cli_opts.optflag("", "no-strip", "Do not strip debug symbols from the binary");
    cli_opts.optflag("", "strip", "Always try to strip debug symbols");
    cli_opts.optflag("", "no-separate-debug-symbols", "Do not strip debug symbols into a separate .debug file");
    cli_opts.optflag("", "separate-debug-symbols", "Strip debug symbols into a separate .debug file");
    cli_opts.optflag("", "compress-debug-symbols", "Apply objcopy --compress-debug-sections");
    cli_opts.optopt("o", "output", "Write .deb to this file or directory", "path");
    cli_opts.optopt("p", "package", "Select which Cargo workspace package to use", "name");
    cli_opts.optflag("", "install", "Immediately install the created deb package");
    cli_opts.optflag("q", "quiet", "Don't print warnings");
    cli_opts.optflag("v", "verbose", "Print progress");
    cli_opts.optflag("", "version", "Show version of the cargo-deb tool");
    cli_opts.optopt("", "deb-version", "Override version string for the package", "version");
    cli_opts.optopt("", "deb-revision", "Override revision suffix string for the package", "num");
    cli_opts.optopt("", "maintainer", "Override Maintainer field", "name");
    cli_opts.optopt("", "manifest-path", "Cargo project file location", "./Cargo.toml");
    cli_opts.optflag("", "offline", "Passed to Cargo");
    cli_opts.optflag("", "locked", "Passed to Cargo");
    cli_opts.optflag("", "frozen", "Passed to Cargo");
    cli_opts.optopt("", "variant", "Alternative Cargo.toml configuration section to use", "name");
    cli_opts.optopt("", "target", "Rust target for cross-compilation", "triple");
    cli_opts.optopt("", "profile", "Select which Cargo build profile to use", "release|<custom>");
    cli_opts.optflag("", "no-build", "Assume the project is already built");
    cli_opts.optopt("", "cargo-build", "Override cargo build subcommand", "subcommand");
    cli_opts.optflag("", "fast", "Use faster compression, which makes a larger deb file");
    cli_opts.optopt("Z", "compress-type", "Compress with the given compression format", "gz|xz");
    cli_opts.optflag("", "compress-system", "Use the corresponding command-line tool for compression");
    cli_opts.optflag("", "system-xz", "Compress using command-line xz command instead of built-in. Deprecated, use --compress-system instead");
    cli_opts.optflag("", "rsyncable", "Use worse compression, but reduce differences between versions of packages");
    cli_opts.optflag("h", "help", "Print this help menu");

    let mut matches = match cli_opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(err) => {
            eprintln!("cargo-deb: Error parsing arguments. See --help for details.");

            use getopts::Fail::*;
            let error_arg = match &err {
                ArgumentMissing(s) | UnrecognizedOption(s) | OptionMissing(s) |
                OptionDuplicated(s) | UnexpectedArgument(s) => s,
            };
            let dym = cli_opts.usage_with_format(|opts| {
                let mut out = String::new();
                for o in opts.filter(|o| error_arg.split('-').filter(|e| !e.is_empty()).any(|e| o.contains(e))) {
                    out.push_str(&o); out.push('\n');
                }
                out
            });
            if !dym.is_empty() {
                eprintln!("Did you mean:\n{dym}");
            }
            print_error(&err);
            return ExitCode::FAILURE;
        },
    };
    if matches.opt_present("h") {
        print!("{}", cli_opts.usage_with_format(|opts| {
            let mut out = String::with_capacity(2000);
            out.push_str("Usage: cargo deb [options] [-- <cargo build flags>]\nhttps://lib.rs/cargo-deb ");
            out.push_str(env!("CARGO_PKG_VERSION"));
            out.push_str("\n\n");
            for opt in opts.filter(|opt| !opt.contains("--system-xz") && !opt.contains("--no-separate-debug-symbols")) {
                out.push_str(&opt);
                out.push('\n');
            }
            out
        }));
        return ExitCode::SUCCESS;
    }

    if matches.opt_present("version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let install = matches.opt_present("install");

    let compress_type = match matches.opt_str("compress-type").as_deref() {
        Some("gz" | "gzip") => Format::Gzip,
        Some("xz") | None => Format::Xz,
        _ => {
            print_error(&CargoDebError::Str("unrecognized compression format. Supported: gzip, xz"));
            return ExitCode::FAILURE;
        },
    };

    // `cargo deb` invocation passes the `deb` arg through.
    if matches.free.first().is_some_and(|arg| arg == "deb") {
        matches.free.remove(0);
    }

    let quiet = matches.opt_present("quiet");
    let verbose = matches.opt_present("verbose") || env::var_os("RUST_LOG").is_some_and(|v| v == "debug");

    // Listener conditionally prints warnings
    let (listener_tmp1, listener_tmp2);
    let listener: &dyn listener::Listener = if quiet {
        listener_tmp1 = listener::NoOpListener;
        &listener_tmp1
    } else {
        listener_tmp2 = listener::StdErrListener { verbose };
        &listener_tmp2
    };

    let deb_version = matches.opt_str("deb-version");
    let deb_revision = matches.opt_str("deb-revision");

    if deb_version.is_some() && deb_revision.as_deref().is_some_and(|r| !r.is_empty()) {
        listener.warning(format!("--deb-version takes precedence over --deb-revision. Revision '{}' will be ignored", deb_revision.as_deref().unwrap_or_default()));
    }

    match CargoDeb::new(CargoDebOptions {
        no_build: matches.opt_present("no-build"),
        strip_override: if matches.opt_present("strip") { Some(true) } else if matches.opt_present("no-strip") { Some(false) } else { None },
        separate_debug_symbols: if matches.opt_present("separate-debug-symbols") { Some(true) } else if matches.opt_present("no-separate-debug-symbols") { Some(false) } else { None },
        compress_debug_symbols: if matches.opt_present("compress-debug-symbols") { Some(true) } else { None },
        verbose,
        install,
        // when installing locally it won't be transferred anywhere, so allow faster compression
        fast: install || matches.opt_present("fast"),
        variant: matches.opt_str("variant"),
        target: matches.opt_str("target"),
        output_path: matches.opt_str("output"),
        selected_package_name: matches.opt_str("package"),
        manifest_path: matches.opt_str("manifest-path"),
        overrides: cargo_deb::config::DebConfigOverrides {
            deb_version,
            deb_revision,
            maintainer: matches.opt_str("maintainer"),
        },
        compress_type,
        compress_system: matches.opt_present("compress-system"),
        system_xz: matches.opt_present("system-xz"),
        rsyncable: matches.opt_present("rsyncable"),
        profile: matches.opt_str("profile"),
        cargo_build_cmd: matches.opt_str("cargo-build").unwrap_or("build".to_string()),
        cargo_locking_flags: CargoLockingFlags {
            offline: matches.opt_present("offline"),
            frozen: matches.opt_present("frozen"),
            locked: matches.opt_present("locked"),
        },
        cargo_build_flags: matches.free,
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
