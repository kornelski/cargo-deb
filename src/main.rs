#![allow(clippy::redundant_closure_for_method_calls)]

use cargo_deb::control::ControlArchiveBuilder;
use cargo_deb::assets::DebugSymbols;
use cargo_deb::*;
use std::env;
use std::path::Path;
use std::process::ExitCode;

struct CliOptions {
    no_build: bool,
    strip_override: Option<bool>,
    separate_debug_symbols: Option<bool>,
    compress_debug_symbols: Option<bool>,
    fast: bool,
    verbose: bool,
    quiet: bool,
    install: bool,
    selected_package_name: Option<String>,
    output_path: Option<String>,
    variant: Option<String>,
    target: Option<String>,
    manifest_path: Option<String>,
    cargo_build_cmd: String,
    cargo_build_flags: Vec<String>,
    deb_version: Option<String>,
    deb_revision: Option<String>,
    compress_type: compress::Format,
    compress_system: bool,
    system_xz: bool,
    rsyncable: bool,
    profile: Option<String>,
}

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
    cli_opts.optopt("", "deb-version", "Alternate version string for the package", "version");
    cli_opts.optopt("", "deb-revision", "Alternate revision suffix string for the package", "num");
    cli_opts.optopt("", "manifest-path", "Cargo project file location", "./Cargo.toml");
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

    let matches = match cli_opts.parse(&args[1..]) {
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
        }
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
        Some("gz" | "gzip") => compress::Format::Gzip,
        Some("xz") | None => compress::Format::Xz,
        _ => {
            print_error(&CargoDebError::Str("unrecognized compression format. Supported: gzip, xz"));
            return ExitCode::FAILURE;
        }
    };

    match process(CliOptions {
        no_build: matches.opt_present("no-build"),
        strip_override: if matches.opt_present("strip") { Some(true) } else if matches.opt_present("no-strip") { Some(false) } else { None },
        separate_debug_symbols: if matches.opt_present("separate-debug-symbols") { Some(true) } else if matches.opt_present("no-separate-debug-symbols") { Some(false) } else { None },
        compress_debug_symbols: if matches.opt_present("compress-debug-symbols") { Some(true) } else { None },
        quiet: matches.opt_present("quiet"),
        verbose: matches.opt_present("verbose") || std::env::var_os("RUST_LOG").is_some_and(|v| v == "debug"),
        install,
        // when installing locally it won't be transferred anywhere, so allow faster compression
        fast: install || matches.opt_present("fast"),
        variant: matches.opt_str("variant"),
        target: matches.opt_str("target"),
        output_path: matches.opt_str("output"),
        selected_package_name: matches.opt_str("package"),
        manifest_path: matches.opt_str("manifest-path"),
        deb_version: matches.opt_str("deb-version"),
        deb_revision: matches.opt_str("deb-revision"),
        compress_type,
        compress_system: matches.opt_present("compress-system"),
        system_xz: matches.opt_present("system-xz"),
        rsyncable: matches.opt_present("rsyncable"),
        profile: matches.opt_str("profile"),
        cargo_build_cmd: matches.opt_str("cargo-build").unwrap_or("build".to_string()),
        cargo_build_flags: matches.free,
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            print_error(&err);
            ExitCode::FAILURE
        }
    }
}

#[allow(deprecated)]
fn err_cause(err: &dyn std::error::Error, max: usize) {
    if let Some(reason) = err.cause() { // we use cause(), not source()
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

fn process(
    CliOptions {
        manifest_path,
        output_path,
        selected_package_name,
        variant,
        target,
        install,
        no_build,
        strip_override,
        separate_debug_symbols,
        compress_debug_symbols,
        quiet,
        fast,
        verbose,
        cargo_build_cmd,
        mut cargo_build_flags,
        deb_version,
        deb_revision,
        mut compress_type,
        mut compress_system,
        system_xz,
        rsyncable,
        profile,
    }: CliOptions,
) -> CDResult<()> {
    let target = target.as_deref();
    let variant = variant.as_deref();

    if install || target.is_none() {
        warn_if_not_linux(); // compiling natively for non-linux = nope
    }

    // `cargo deb` invocation passes the `deb` arg through.
    if cargo_build_flags.first().map_or(false, |arg| arg == "deb") {
        cargo_build_flags.remove(0);
    }

    // Listener conditionally prints warnings
    let listener_tmp1;
    let listener_tmp2;
    let listener: &dyn listener::Listener = if quiet {
        listener_tmp1 = listener::NoOpListener;
        &listener_tmp1
    } else {
        listener_tmp2 = listener::StdErrListener { verbose };
        &listener_tmp2
    };

    if system_xz {
        listener.warning("--system-xz is deprecated, use --compress-system instead.".into());

        compress_type = compress::Format::Xz;
        compress_system = true;
    }

    // The profile is selected based on the given ClI options and then passed to
    // cargo build accordingly. you could argue that the other way around is
    // more desirable. However for now we want all commands coming in via the
    // same `interface`
    let selected_profile = profile.as_deref().unwrap_or("release");
    if selected_profile == "dev" {
        listener.warning("dev profile is not supported and will be a hard error in the future. \
            cargo-deb is for making releases, and it doesn't make sense to use it with dev profiles.".into());
        listener.warning("To enable debug symbols set `[profile.release] debug = true` instead.".into());
    }
    cargo_build_flags.push(format!("--profile={selected_profile}"));

    let root_manifest_path = manifest_path.as_deref().map(Path::new);
    let mut options = Config::from_manifest(
        root_manifest_path,
        selected_package_name.as_deref(),
        output_path,
        target,
        variant,
        deb_version,
        deb_revision,
        listener,
        selected_profile,
        separate_debug_symbols,
        compress_debug_symbols,
    )?;
    reset_deb_temp_directory(&options)?;

    options.extend_cargo_build_flags(&mut cargo_build_flags);

    if !no_build {
        cargo_build(&options, target, &cargo_build_cmd, &cargo_build_flags, verbose)?;
    }

    options.resolve_assets()?;

    crate::data::compress_assets(&mut options, listener)?;

    if strip_override.unwrap_or(options.debug_symbols != DebugSymbols::Keep) {
        strip_binaries(&mut options, target, listener)?;
    } else {
        log::debug!("not stripping debug={:?} strip-flag={:?}", options.debug_symbols, strip_override);
    }

    options.sort_assets_by_type();

    // Obtain the current time which will be used to stamp the generated files in the archives.
    let default_timestamp = options.default_timestamp;

    let options = &options;
    let (control_builder, data_result) = rayon::join(
        move || {
            // The control archive is the metadata for the package manager
            let mut control_builder = ControlArchiveBuilder::new(compress::select_compressor(fast, compress_type, compress_system)?, default_timestamp, listener);
            control_builder.generate_archive(options)?;
            Ok::<_, CargoDebError>(control_builder)
        },
        move || {
            // Initialize the contents of the data archive (files that go into the filesystem).
            let (compressed, asset_hashes) = data::generate_archive(compress::select_compressor(fast, compress_type, compress_system)?, options, default_timestamp, rsyncable, listener)?;
            let original_data_size = compressed.uncompressed_size;
            Ok::<_, CargoDebError>((compressed.finish()?, original_data_size, asset_hashes))
        },
    );
    let mut control_builder = control_builder?;
    let (data_compressed, original_data_size, asset_hashes) = data_result?;
    control_builder.generate_sha256sums(options, asset_hashes)?;
    let control_compressed = control_builder.finish()?.finish()?;

    let mut deb_contents = DebArchive::new(options)?;
    deb_contents.add_data("debian-binary".into(), default_timestamp, b"2.0\n")?;

    // Order is important for Debian
    deb_contents.add_data(format!("control.tar.{}", control_compressed.extension()), default_timestamp, &control_compressed)?;
    drop(control_compressed);
    let compressed_data_size = data_compressed.len();
    listener.info(format!(
        "compressed/original ratio {compressed_data_size}/{original_data_size} ({}%)",
        compressed_data_size * 100 / original_data_size
    ));
    deb_contents.add_data(format!("data.tar.{}", data_compressed.extension()), default_timestamp, &data_compressed)?;
    drop(data_compressed);

    let generated = deb_contents.finish()?;
    if !quiet {
        println!("{}", generated.display());
    }

    remove_deb_temp_directory(options);

    if install {
        install_deb(&generated)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn warn_if_not_linux() {}

#[cfg(not(target_os = "linux"))]
fn warn_if_not_linux() {
    const DEFAULT_TARGET: &str = env!("CARGO_DEB_DEFAULT_TARGET");
    eprintln!("warning: You're creating a package for your current operating system only ({DEFAULT_TARGET}), and not for Linux.\nUse --target if you want to cross-compile.");
}
