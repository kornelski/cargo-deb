use cargo_deb::*;
use std::env;
use std::path::Path;
use std::process;
use std::time;

struct CliOptions {
    no_build: bool,
    strip_override: Option<bool>,
    separate_debug_symbols: bool,
    fast: bool,
    verbose: bool,
    quiet: bool,
    install: bool,
    package_name: Option<String>,
    output_path: Option<String>,
    variant: Option<String>,
    target: Option<String>,
    manifest_path: Option<String>,
    cargo_build_flags: Vec<String>,
    deb_version: Option<String>,
    deb_revision: Option<String>,
    system_xz: bool,
    profile: Option<String>,
}

fn main() {
    env_logger::init();

    let args: Vec<String> = env::args().collect();

    let mut cli_opts = getopts::Options::new();
    cli_opts.optflag("", "no-build", "Assume project is already built");
    cli_opts.optflag("", "no-strip", "Do not strip debug symbols from the binary");
    cli_opts.optflag("", "strip", "Always try to strip debug symbols");
    cli_opts.optflag("", "separate-debug-symbols", "Strip debug symbols into a separate .debug file");
    cli_opts.optflag("", "fast", "Use faster compression, which yields larger archive");
    cli_opts.optflag("", "install", "Immediately install created package");
    cli_opts.optopt("", "target", "Rust target for cross-compilation", "triple");
    cli_opts.optopt("", "variant", "Alternative configuration section to use", "name");
    cli_opts.optopt("", "manifest-path", "Cargo project file location", "./Cargo.toml");
    cli_opts.optopt("p", "package", "Select one of packages belonging to a workspace", "name");
    cli_opts.optopt("o", "output", "Write .deb to this file or directory", "path");
    cli_opts.optflag("q", "quiet", "Don't print warnings");
    cli_opts.optflag("v", "verbose", "Print progress");
    cli_opts.optflag("h", "help", "Print this help menu");
    cli_opts.optflag("", "version", "Show the version of cargo-deb");
    cli_opts.optopt("", "deb-version", "Alternate version string for package", "version");
    cli_opts.optopt("", "deb-revision", "Alternate revision string for package", "revision");
    cli_opts.optflag("", "system-xz", "Compress using command-line xz command instead of built-in");
    cli_opts.optopt("", "profile", "select which project profile to package", "profile");

    let matches = match cli_opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(err) => {
            err_exit(&err);
        },
    };
    if matches.opt_present("h") {
        print!("{}", cli_opts.usage("Usage: cargo deb [options] [-- <cargo build flags>]"));
        return;
    }

    if matches.opt_present("version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let install = matches.opt_present("install");
    match process(CliOptions {
        no_build: matches.opt_present("no-build"),
        strip_override: if matches.opt_present("strip") { Some(true) } else if matches.opt_present("no-strip") { Some(false) } else { None },
        separate_debug_symbols: matches.opt_present("separate-debug-symbols"),
        quiet: matches.opt_present("quiet"),
        verbose: matches.opt_present("verbose"),
        install,
        // when installing locally it won't be transferred anywhere, so allow faster compression
        fast: install || matches.opt_present("fast"),
        variant: matches.opt_str("variant"),
        target: matches.opt_str("target"),
        output_path: matches.opt_str("output"),
        package_name: matches.opt_str("package"),
        manifest_path: matches.opt_str("manifest-path"),
        deb_version: matches.opt_str("deb-version"),
        deb_revision: matches.opt_str("deb-revision"),
        system_xz: matches.opt_present("system-xz"),
        profile: matches.opt_str("profile"),
        cargo_build_flags: matches.free,
    }) {
        Ok(()) => {},
        Err(err) => {
            err_exit(&err);
        }
    }
}

#[allow(deprecated)]
fn err_cause(err: &dyn std::error::Error, max: usize) {
    if let Some(reason) = err.cause() { // we use cause(), not source()
        eprintln!("  because: {}", reason);
        if max > 0 {
            err_cause(reason, max - 1);
        }
    }
}

fn err_exit(err: &dyn std::error::Error) -> ! {
    eprintln!("cargo-deb: {}", err);
    err_cause(err, 3);
    process::exit(1);
}

fn process(
    CliOptions {
        manifest_path,
        output_path,
        package_name,
        variant,
        target,
        install,
        no_build,
        strip_override,
        separate_debug_symbols,
        quiet,
        fast,
        verbose,
        mut cargo_build_flags,
        deb_version,
        deb_revision,
        system_xz,
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

    // The profile is selected based on the given ClI options and then passed to
    // cargo build accordingly. you could argue that the other way around is
    // more desireable. However for now we want all commands coming in via the
    // same `interface`
    let selected_profile = profile.unwrap_or("release".to_string());
    cargo_build_flags.push("--profile".to_string());
    cargo_build_flags.push(selected_profile.clone());

    let manifest_path = manifest_path.as_ref().map_or("Cargo.toml", |s| s.as_str());
    let mut options = Config::from_manifest(
        Path::new(manifest_path),
        package_name.as_deref(),
        output_path,
        target,
        variant,
        deb_version,
        deb_revision,
        listener,
        selected_profile,
    )?;
    reset_deb_temp_directory(&options)?;

    if !no_build {
        cargo_build(&options, target, &cargo_build_flags, verbose)?;
    }

    options.resolve_assets()?;

    crate::data::compress_assets(&mut options, listener)?;

    if strip_override.unwrap_or(separate_debug_symbols || !options.debug_enabled) {
        strip_binaries(&mut options, target, listener, separate_debug_symbols)?;
    } else {
        log::debug!("not stripping profile.release.debug={} strip-flag={:?}", options.debug_enabled, strip_override);
    }

    // Obtain the current time which will be used to stamp the generated files in the archives.
    let system_time = time::SystemTime::now().duration_since(time::UNIX_EPOCH)?.as_secs();
    let mut deb_contents = DebArchive::new(&options)?;

    deb_contents.add_data("debian-binary", system_time, b"2.0\n")?;

    // Initailize the contents of the data archive (files that go into the filesystem).
    let (data_archive, asset_hashes) = data::generate_archive(&options, system_time, listener)?;
    let original = data_archive.len();

    let listener_tmp = &*listener; // reborrow for the closure
    let options = &options;
    let (control_compressed, data_compressed) = rayon::join(move || {
        // The control archive is the metadata for the package manager
        let control_archive = control::generate_archive(options, system_time, asset_hashes, listener_tmp)?;
        compress::xz_or_gz(&control_archive, fast, system_xz)
    }, move || {
        compress::xz_or_gz(&data_archive, fast, system_xz)
    });
    let control_compressed = control_compressed?;
    let data_compressed = data_compressed?;

    // Order is important for Debian
    deb_contents.add_data(&format!("control.tar.{}", control_compressed.extension()), system_time, &control_compressed)?;
    drop(control_compressed);
    let compressed = data_compressed.len();
    listener.info(format!(
        "compressed/original ratio {}/{} ({}%)",
        compressed,
        original,
        compressed * 100 / original
    ));
    deb_contents.add_data(&format!("data.tar.{}", data_compressed.extension()), system_time, &data_compressed)?;
    drop(data_compressed);

    let generated = deb_contents.finish()?;
    if !quiet {
        println!("{}", generated.display());
    }

    remove_deb_temp_directory(&options);

    if install {
        install_deb(&generated)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn warn_if_not_linux() {}

#[cfg(not(target_os = "linux"))]
fn warn_if_not_linux() {
    eprintln!("warning: This command is for Linux only, and will not make sense when run on other systems");
}
