use quick_error::quick_error;
use std::path::PathBuf;
use std::{io, num, time};

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
            display("{}: {}", msg, file.display())
            source(err)
        }
        CommandFailed(err: io::Error, cmd: &'static str) {
            display("Command {} failed to launch", cmd)
            source(err)
        }
        CommandError(msg: &'static str, arg: String, reason: Vec<u8>) {
            display("{} ({}): {}", msg, arg, String::from_utf8_lossy(reason))
        }
        Str(msg: &'static str) {
            display("{}", msg)
            from()
        }
        NumParse(msg: &'static str, err: num::ParseIntError) {
            display("{}", msg)
            source(err)
        }
        InvalidVersion(msg: &'static str, ver: String) {
            display("Version '{}' is invalid: {}", ver, msg)
        }
        InstallFailed {
            display("Installation failed, because dpkg -i returned error")
        }
        BuildFailed {
            display("Build failed")
        }
        DebHelperReplaceFailed(name: PathBuf) {
            display("Unable to replace #DEBHELPER# token in maintainer script '{}'", name.display())
        }
        StripFailed(name: PathBuf, reason: String) {
            display("Unable to strip binary '{}': {}", name.display(), reason)
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
        ParseUTF8(err: std::str::Utf8Error) {
            from()
            from(err: std::string::FromUtf8Error) -> (err.utf8_error())
        }
        PackageNotFound(path: String, reason: Vec<u8>) {
            display("Path '{}' does not belong to a package: {}", path, String::from_utf8_lossy(reason))
        }
        BinariesNotFound(crate_name: String) {
            display("No binaries or cdylibs found. The package `{crate_name}` empty. Please specify some assets to package in Cargo.toml")
        }
        PackageNotFoundInWorkspace(name: String, available: String) {
            display("The workspace doesn't have a package named {name}.\nAvailable packages are: {available}")
        }
        NoRootFoundInWorkspace(available: String) {
            display("This is a workspace with multiple packages, and there is no single package at the root.\nPlease specify the package with `-p` or set one in the workspace's `default-members = []`.\nAvailable packages are: {available}")
        }
        VariantNotFound(variant: String) {
            display("[package.metadata.deb.variants.{}] not found in Cargo.toml", variant)
        }
        GlobPatternError(err: glob::PatternError) {
            from()
            display("Unable to parse glob pattern")
            source(err)
        }
        AssetFileNotFound(path: PathBuf) {
            display("Static file asset path or glob pattern did not match any existing files: {}", path.display())
        }
        AssetGlobError(err: glob::GlobError) {
            from()
            display("Unable to iterate asset glob result")
            source(err)
        }
        #[cfg(feature = "lzma")]
        LzmaCompressionError(err: xz2::stream::Error) {
            display("Lzma compression error: {:?}", err)
        }
    }
}

pub type CDResult<T> = Result<T, CargoDebError>;
