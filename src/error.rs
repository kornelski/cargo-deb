use quick_error::quick_error;
use std::borrow::Cow;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::{env, fmt, io, num, time};

quick_error! {
    #[derive(Debug)]
    #[non_exhaustive]
    pub enum CargoDebError {
        Io(err: io::Error) {
            from()
            display("I/O error: {}", err)
            source(err)
        }
        TomlParsing(err: cargo_toml::Error, path: PathBuf) {
            display("Unable to parse {}", path.display())
            source(err)
        }
        IoFile(msg: &'static str, err: io::Error, file: PathBuf) {
            display("{msg}: {}{}{}",
                file.display(),
                file.is_relative().then(|| env::current_dir().ok()).flatten().map(|cwd| format!("\nnote: The current dir is '{}'", cwd.display())).unwrap_or_default(),
                file.ancestors().find(|p| p.exists() && p.parent().is_some()).map(|p| format!("\nnote: '{}' exists", p.display())).unwrap_or_default(),
            )
            source(err)
        }
        CommandFailed(err: io::Error, cmd: Cow<'static, str>) {
            display("Command `{cmd}` failed to launch\nnote: The current $PATH is {}", env::var("PATH").as_deref().unwrap_or("unset or invalid"))
            source(err)
        }
        CommandError(msg: &'static str, arg: String, reason: Vec<u8>) {
            display("{msg} ({arg}): {}", String::from_utf8_lossy(reason).trim_start_matches("error: "))
        }
        Str(msg: &'static str) {
            display("{msg}")
            from()
        }
        NumParse(msg: &'static str, err: num::ParseIntError) {
            display("{msg}")
            source(err)
        }
        InvalidVersion(msg: &'static str, ver: String) {
            display("Version '{ver}' is invalid: {msg}")
        }
        InstallFailed(status: ExitStatus) {
            display("Installation failed, because `dpkg -i` returned error {status}")
        }
        BuildFailed {
            display("Build failed")
        }
        DebHelperReplaceFailed(name: PathBuf) {
            display("Unable to replace #DEBHELPER# token in maintainer script '{}'", name.display())
        }
        StripFailed(name: PathBuf, reason: String) {
            display("Unable to strip binary '{}': {reason}", name.display())
        }
        SystemTime(err: time::SystemTimeError) {
            from()
            display("Unable to get system time")
            source(err)
        }
        ParseTOML(err: toml::de::Error) {
            from()
            display("Unable to parse Cargo.toml")
            source(err)
        }
        ParseJSON(err: serde_json::Error) {
            from()
            display("Unable to parse `cargo metadata` output")
            source(err)
        }
        PackageNotFound(path: String, reason: Vec<u8>) {
            display("Path '{path}' does not belong to a package: {}", String::from_utf8_lossy(reason))
        }
        BinariesNotFound(crate_name: String) {
            display("No binaries or cdylibs found. The package '{crate_name}' is empty. Please specify some assets to package in Cargo.toml")
        }
        PackageNotFoundInWorkspace(name: String, available: String) {
            display("The workspace doesn't have a package named {name}.\nnote: Available packages are: {available}")
        }
        NoRootFoundInWorkspace(available: String) {
            display("This is a workspace with multiple packages, and there is no single package at the root.\nPlease specify the package with `-p` or set one in the workspace's `default-members = []`.\nnote: Available packages are: {available}")
        }
        VariantNotFound(variant: String) {
            display("[package.metadata.deb.variants.{}] not found in Cargo.toml", variant)
        }
        GlobPatternError(err: glob::PatternError) {
            from()
            display("Unable to parse glob pattern")
            source(err)
        }
        AssetFileNotFound(source_path: PathBuf, target_path: PathBuf, is_glob: bool, is_built: bool) {
            display("{} {}: {}\nNeeded for {}",
                if *is_glob { "Glob pattern" } else { "Static file asset" },
                if *is_built { "has not been built" } else { "path did not match any existing files" },
                source_path.display(),
                target_path.display(),
            )
        }
        AssetGlobError(err: glob::GlobError) {
            from()
            display("Unable to iterate asset glob result")
            source(err)
        }
        Context(msg: String, err: Box<CargoDebError>) {
            display("{msg}")
            source(err)
        }
        #[cfg(feature = "lzma")]
        LzmaCompressionError(err: xz2::stream::Error) {
            display("Lzma compression error: {:?}", err)
        }
    }
}

impl CargoDebError {
    pub(crate) fn context(self, msg: impl fmt::Display) -> Self {
        Self::Context(msg.to_string(), Box::new(self))
    }
}

impl From<fmt::Error> for CargoDebError {
    fn from(_: fmt::Error) -> Self {
        Self::Str("fmt")
    }
}

pub type CDResult<T> = Result<T, CargoDebError>;
