use crate::config::{is_glob_pattern, PackageConfig};
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::parse::manifest::CargoDebAssetArrayOrTable;
use crate::util::compress::gzipped;
use crate::util::read_file_to_bytes;
use rayon::prelude::*;
use std::borrow::Cow;
use std::env::consts::DLL_SUFFIX;
use std::path::{Path, PathBuf};
use std::{fmt, fs};


#[derive(Debug, Clone)]
pub enum AssetSource {
    /// Copy file from the path (and strip binary if needed).
    Path(PathBuf),
    /// A symlink existing in the file system or specified in the manifest
    Symlink(SymlinkKind),
    /// Write data to destination as-is.
    Data(Vec<u8>),
}

#[derive(Debug, Clone)]
pub enum SymlinkKind {
    /// A symlink existing in the file system
    /// If reserve symlinks is enabled the links source path will be read from this path,
    /// otherwise the file content of the symlink destination will be added as a normal file
    Manifested(PathBuf),
    /// A symlink specified in the manifest
    /// The path is the destination of the symlink
    Virtual(PathBuf),
}

impl AssetSource {
    /// Symlink must exist on disk to be preserved
    #[must_use]
    pub fn from_path(path: impl Into<PathBuf>, preserve_existing_symlink: bool) -> Self {
        let path = path.into();
        if preserve_existing_symlink || !path.exists() { // !exists means a symlink to bogus path
            if let Ok(md) = fs::symlink_metadata(&path) {
                if md.is_symlink() {
                    return Self::Symlink(SymlinkKind::Manifested(path));
                }
            }
        }
        Self::Path(path)
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Symlink(SymlinkKind::Manifested(p)) |
            Self::Path(p) => Some(p),
            Self::Data(_) | 
            Self::Symlink(SymlinkKind::Virtual(_)) => None ,
        }
    }

    #[must_use]
    pub fn into_path(self) -> Option<PathBuf> {
        match self {
            Self::Symlink(SymlinkKind::Manifested(p)) |
            Self::Path(p) => Some(p),
            Self::Data(_) | 
            Self::Symlink(SymlinkKind::Virtual(_))  => None,
        }
    }

    #[must_use]
    pub fn archive_as_symlink_only(&self) -> bool {
        matches!(self, Self::Symlink(_))
    }

    #[must_use]
    pub fn file_size(&self) -> Option<u64> {
        match *self {
            Self::Path(ref p) => fs::metadata(p).ok().map(|m| m.len()),
            Self::Data(ref d) => Some(d.len() as u64),
            Self::Symlink(_)  => None,
        }
    }

    pub fn data(&self) -> CDResult<Cow<'_, [u8]>> {
        Ok(match self {
            Self::Path(p) => {
                let data = read_file_to_bytes(p)
                    .map_err(|e| CargoDebError::IoFile("Unable to read asset to add to archive", e, p.clone()))?;
                Cow::Owned(data)
            },
            Self::Data(d) => Cow::Borrowed(d),
            Self::Symlink(SymlinkKind::Manifested(p))  => {
                let data = read_file_to_bytes(p)
                    .map_err(|e| CargoDebError::IoFile("Symlink unexpectedly used to read file data", e, p.clone()))?;
                Cow::Owned(data)
            },
            Self::Symlink(SymlinkKind::Virtual(p)) => {
                return Err(CargoDebError::CannotReadVirtualSymlink(p.clone()))
            }
        })
    }

    pub(crate) fn magic_bytes(&self) -> Option<[u8; 4]> {
        match self {
            Self::Path(p) | Self::Symlink(SymlinkKind::Manifested(p)) => {
                let mut buf = [0; 4];
                use std::io::Read;
                let mut file = std::fs::File::open(p).ok()?;
                file.read_exact(&mut buf[..]).ok()?;
                Some(buf)
            },
            Self::Data(d) => {
                d.get(..4).and_then(|b| b.try_into().ok())
            },
            Self::Symlink(SymlinkKind::Virtual(_)) => None
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Assets {
    pub unresolved: Vec<UnresolvedAsset>,
    pub resolved: Vec<Asset>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(try_from = "CargoDebAssetArrayOrTable")]
pub(crate) enum RawAssetOrAuto {
    Auto,
    RawAsset(RawAsset),
}

impl RawAssetOrAuto {
    pub fn asset(self) -> Option<RawAsset> {
        match self {
            Self::RawAsset(a) => Some(a),
            Self::Auto => None,
        }
    }
}

impl From<RawAsset> for RawAssetOrAuto {
    fn from(r: RawAsset) -> Self {
        Self::RawAsset(r)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(try_from = "RawAssetOrAuto")]
pub(crate) struct RawAsset {
    pub source_path: PathBuf,
    pub target_path: PathBuf,
    pub chmod: Option<u32>,
}

impl TryFrom<RawAssetOrAuto> for RawAsset {
    type Error = &'static str;

    fn try_from(maybe_auto: RawAssetOrAuto) -> Result<Self, Self::Error> {
        maybe_auto.asset().ok_or("$auto is not allowed here")
    }
}

impl Assets {
    pub(crate) const fn new(unresolved: Vec<UnresolvedAsset>, resolved: Vec<Asset>) -> Self {
        Self {
            unresolved,
            resolved,
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &AssetCommon> {
        self.resolved.iter().map(|u| &u.c).chain(self.unresolved.iter().map(|r| &r.c))
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AssetKind {
    Any,
    CargoExampleBinary,
    SeparateDebugSymbols,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IsBuilt {
    No,
    SamePackage,
    /// needs --workspace to build
    Workspace,
}

fn get_file_mode(path: &Path) -> CDResult<u32> {
    
    #[cfg(not(unix))]
    {
        Err(CargoDebError::ImplicitFileModeFromPathNotSupported(path.to_path_buf()))
    }
    
    #[cfg(unix)]
    {
        let metadata = fs::metadata(path)
            .map_err(|e| CargoDebError::IoFile(
                "Unable to read file metadata for permissions", 
                e, 
                path.to_owned()
            ))?;
        use std::os::unix::fs::PermissionsExt;
        Ok(metadata.permissions().mode() & 0o7777)
    }
}

#[derive(Debug, Clone)]
pub struct UnresolvedAsset {
    pub source_path: PathBuf,
    pub c: AssetCommon,
}

impl UnresolvedAsset {
    pub(crate) fn new(source_path: PathBuf, target_path: PathBuf, chmod: Option<u32>, is_built: IsBuilt, asset_kind: AssetKind) -> Self {
        Self {
            source_path,
            c: AssetCommon { target_path, chmod, asset_kind, is_built },
        }
    }

    /// Convert `source_path` (with glob or dir) to actual path
    pub fn resolve(&self, preserve_symlinks: bool) -> CDResult<Vec<Asset>> {
        let Self { ref source_path, c: AssetCommon { ref target_path, chmod, is_built, asset_kind } } = *self;

        let source_prefix_len = is_glob_pattern(source_path.as_os_str()).then(|| {
            let file_name_is_glob = source_path
                .file_name()
                .is_some_and(is_glob_pattern);

            if file_name_is_glob {
                // skip to the component before the glob
                let glob_component_pos = source_path
                    .parent()
                    .and_then(|parent| parent.iter().position(is_glob_pattern));
                glob_component_pos.unwrap_or_else(|| {
                    source_path
                        .iter()
                        .count()
                })
            } else {
                // extract the only file name component
                source_path
                    .iter()
                    .count()
                    .saturating_sub(1)
            }
        });

        let matched_assets = glob::glob(source_path.to_str().ok_or("utf8 path")?)?
            // Remove dirs from globs without throwing away errors
            .map(|entry| {
                let source_file = entry?;
                Ok(if source_file.is_dir() { None } else { Some(source_file) })
            })
            .filter_map(|res: Result<Option<PathBuf>, glob::GlobError>| {
                Some(res.transpose()?.map_err(CargoDebError::from).and_then(|source_file| {
                    let target_file = if let Some(source_prefix_len) = source_prefix_len {
                        target_path.join(
                            source_file
                            .iter()
                            .skip(source_prefix_len)
                            .collect::<PathBuf>())
                    } else {
                        target_path.clone()
                    };
                    // Use provided chmod or read from filesystem
                    let file_chmod = match chmod {
                        Some(chmod) => chmod,
                        None => get_file_mode(&source_file)?,
                    };
                    log::debug!("asset {} -> {} {} {:o}", source_file.display(), target_file.display(), if is_built != IsBuilt::No {"copy"} else {"build"}, file_chmod);
                    
                    let asset = Asset::new(
                        AssetSource::from_path(source_file, preserve_symlinks),
                        target_file,
                        Some(file_chmod),
                        is_built,
                        asset_kind,
                    );
                    if source_prefix_len.is_some() {
                        Ok(asset.processed("glob", None))
                    } else {
                        Ok(asset)
                    }
                }))
            })
            .collect::<CDResult<Vec<_>>>()
            .map_err(|e| e.context(format_args!("Error while glob searching {}", source_path.display())))?;

        if matched_assets.is_empty() {
            return Err(CargoDebError::AssetFileNotFound(
                source_path.clone(),
                Asset::normalized_target_path(target_path.clone(), Some(source_path)),
                source_prefix_len.is_some(), is_built != IsBuilt::No));
        }
        Ok(matched_assets)
    }
}

#[derive(Debug, Clone)]
pub struct AssetCommon {
    pub target_path: PathBuf,
    pub chmod: Option<u32>,
    pub(crate) asset_kind: AssetKind,
    is_built: IsBuilt,
}

pub(crate) struct AssetFmt<'a> {
    c: &'a AssetCommon,
    cwd: &'a Path,
    source: Option<&'a Path>,
    processed_from: Option<&'a ProcessedFrom>,
}

impl<'a> AssetFmt<'a> {
    pub fn new(asset: &'a Asset, cwd: &'a Path) -> Self {
        Self {
            c: &asset.c,
            source: asset.source.path(),
            processed_from: asset.processed_from.as_ref(),
            cwd,
        }
    }

    pub fn unresolved(asset: &'a UnresolvedAsset, cwd: &'a Path) -> Self {
        Self {
            c: &asset.c,
            source: Some(&asset.source_path),
            processed_from: None,
            cwd,
        }
    }
}

impl fmt::Display for AssetFmt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut src = self.source;
        let action = self.processed_from.map(|proc| {
            src = proc.original_path.as_deref().or(src);
            proc.action
        });
        if let Some(src) = src {
            write!(f, "{} ", src.strip_prefix(self.cwd).unwrap_or(src).display())?;
        }
        if let Some(action) = action {
            write!(f, "({action}{}) ", if self.c.is_built() { "; built" } else { "" })?;
        } else if self.c.is_built() {
            write!(f, "(built) ")?;
        }
        write!(f, "-> {}", self.c.target_path.display())?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub source: AssetSource,
    /// For prettier path display not "/tmp/blah.tmp"
    pub processed_from: Option<ProcessedFrom>,
    pub c: AssetCommon,
}

#[derive(Debug, Clone)]
pub struct ProcessedFrom {
    pub original_path: Option<PathBuf>,
    pub action: &'static str,
}

impl Asset {
    #[must_use]
    pub fn normalized_target_path(mut target_path: PathBuf, source_path: Option<&Path>) -> PathBuf {
        // is_dir() is only for paths that exist
        if target_path.to_string_lossy().ends_with('/') {
            let file_name = source_path.and_then(|p| p.file_name()).expect("source must be a file");
            target_path = target_path.join(file_name);
        }

        if target_path.is_absolute() || target_path.has_root() {
            target_path = target_path.strip_prefix("/").expect("no root dir").to_owned();
        }
        target_path
    }

    #[must_use]
    pub fn new(source: AssetSource, target_path: PathBuf, chmod: Option<u32>, is_built: IsBuilt, asset_kind: AssetKind) -> Self {
        let target_path = Self::normalized_target_path(target_path, source.path());
        Self {
            source,
            processed_from: None,
            c: AssetCommon { target_path, chmod, asset_kind, is_built },
        }
    }

    #[must_use]
    pub fn processed(mut self, action: &'static str, original_path: impl Into<Option<PathBuf>>) -> Self {
        debug_assert!(self.processed_from.is_none());
        self.processed_from = Some(ProcessedFrom {
            original_path: original_path.into(),
            action,
        });
        self
    }

    pub(crate) fn is_binary_executable(&self) -> bool {
        self.c.is_executable()
            && self.c.target_path.extension().is_none_or(|ext| ext != "sh")
            && (self.c.is_built() || self.smells_like_elf())
    }

    fn smells_like_elf(&self) -> bool {
        self.source.magic_bytes().is_some_and(|b| b == [0x7F, b'E', b'L', b'F'])
    }
}

impl AssetCommon {
    pub(crate) const fn is_executable(&self) -> bool {
        if let Some(chmod) = self.chmod {
            0 != chmod & 0o111
        } else {
            false
        }
    }

    pub(crate) fn is_dynamic_library(&self) -> bool {
        is_dynamic_library_filename(&self.target_path)
    }

    pub(crate) fn is_built(&self) -> bool {
        self.is_built != IsBuilt::No
    }

    /// Returns the target path for the debug symbol file, which will be
    /// /usr/lib/debug/<path-to-executable>.debug
    #[must_use]
    pub(crate) fn default_debug_target_path(&self, lib_dir_base: &Path) -> PathBuf {
        // Turn an absolute path into one relative to "/"
        let relative = self.target_path.strip_prefix(Path::new("/"))
            .unwrap_or(self.target_path.as_path());

        // Prepend the debug location and add .debug
        let mut path = Path::new("/").join(lib_dir_base);
        path.push("debug");
        path.push(debug_filename(relative));
        path
    }

    pub(crate) fn is_same_package(&self) -> bool {
        self.is_built == IsBuilt::SamePackage
    }
}

/// Adds `.debug` to the end of a path to a filename
fn debug_filename(path: &Path) -> PathBuf {
    let mut debug_filename = path.as_os_str().to_os_string();
    debug_filename.push(".debug");
    debug_filename.into()
}

pub(crate) fn is_dynamic_library_filename(path: &Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| f.ends_with(DLL_SUFFIX))
}

/// Compress man pages and other assets per Debian Policy.
///
/// # References
///
/// <https://www.debian.org/doc/debian-policy/ch-docs.html>
/// <https://lintian.debian.org/tags/manpage-not-compressed.html>
pub fn compressed_assets(package_deb: &PackageConfig, listener: &dyn Listener) -> CDResult<Vec<(usize, Asset)>> {
    fn needs_compression(path: &str) -> bool {
        !path.ends_with(".gz") &&
            (path.starts_with("usr/share/man/") ||
                (path.starts_with("usr/share/doc/") && (path.ends_with("/NEWS") || path.ends_with("/changelog"))) ||
                (path.starts_with("usr/share/info/") && path.ends_with(".info")))
    }

    package_deb.assets.resolved.iter().enumerate()
        .filter(|(_, asset)| {
            asset.c.target_path.starts_with("usr") && !asset.c.is_built() && needs_compression(&asset.c.target_path.to_string_lossy())
        })
        .par_bridge()
        .map(|(idx, orig_asset)| {
            let mut file_name = orig_asset.c.target_path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            file_name.push_str(".gz");
            let new_path = orig_asset.c.target_path.with_file_name(file_name);
            listener.progress("Compressing", format!("'{}'", new_path.display()));
            let gzdata = gzipped(&orig_asset.source.data()?)
                .map_err(|e| CargoDebError::Io(e).context("error while gzipping asset"))?;
            CDResult::Ok((idx, Asset::new(
                crate::assets::AssetSource::Data(gzdata),
                new_path,
                orig_asset.c.chmod,
                IsBuilt::No,
                AssetKind::Any,
            ).processed("compressed",
                orig_asset.source.path().unwrap_or(&orig_asset.c.target_path).to_path_buf()
            )))
        }).collect()
}

pub fn apply_compressed_assets(package_deb: &mut PackageConfig, new_assets: Vec<(usize, Asset)>) {
    for (idx, asset) in new_assets {
        package_deb.assets.resolved[idx] = asset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BuildEnvironment, BuildOptions, DebConfigOverrides, DebugSymbolOptions};
    use crate::parse::manifest::SystemdUnitsConfig;
    use crate::util::tests::add_test_fs_paths;

    #[test]
    fn assets() {
        let a = Asset::new(
            AssetSource::Path(PathBuf::from("target/release/bar")),
            PathBuf::from("baz/"),
            Some(0o644),
            IsBuilt::SamePackage,
            AssetKind::Any,
        );
        assert_eq!("baz/bar", a.c.target_path.to_str().unwrap());
        assert!(a.c.is_built != IsBuilt::No);

        let a = Asset::new(
            AssetSource::Path(PathBuf::from("foo/bar")),
            PathBuf::from("/baz/quz"),
            Some(0o644),
            IsBuilt::No,
            AssetKind::Any,
        );
        assert_eq!("baz/quz", a.c.target_path.to_str().unwrap());
        assert!(a.c.is_built == IsBuilt::No);
    }

    #[test]
    #[cfg(unix)]
    fn resolve_without_permissions_reads_from_filesystem() {
        // When chmod is None, resolve() should read the file's permissions from disk
        let source_path = PathBuf::from("test-resources/testroot/src/main.rs");
        assert!(source_path.exists(), "test file must exist");

        let asset = UnresolvedAsset {
            source_path: source_path.clone(),
            c: AssetCommon {
                target_path: PathBuf::from("usr/share/test/"),
                chmod: None, // no permissions specified
                asset_kind: AssetKind::Any,
                is_built: IsBuilt::No,
            },
        };

        let resolved = asset.resolve(false).unwrap();
        assert_eq!(resolved.len(), 1);

        let resolved_asset = &resolved[0];
        // The chmod should have been populated from the filesystem
        assert!(resolved_asset.c.chmod.is_some(), "chmod should be read from filesystem when not specified in manifest");

        // Verify the permission value matches what the filesystem reports
        use std::os::unix::fs::PermissionsExt;
        let expected_mode = fs::metadata(&source_path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(resolved_asset.c.chmod.unwrap(), expected_mode);
    }

    #[test]
    fn resolve_with_explicit_permissions_ignores_filesystem() {
        // When chmod is Some, resolve() should use the specified value, not the filesystem
        let source_path = PathBuf::from("test-resources/testroot/src/main.rs");
        assert!(source_path.exists(), "test file must exist");

        let asset = UnresolvedAsset {
            source_path: source_path.clone(),
            c: AssetCommon {
                target_path: PathBuf::from("usr/share/test/"),
                chmod: Some(0o755), // explicit permissions
                asset_kind: AssetKind::Any,
                is_built: IsBuilt::No,
            },
        };

        let resolved = asset.resolve(false).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].c.chmod, Some(0o755), "explicit chmod should be preserved");
    }

    #[test]
    fn assets_globs() {
        for (glob, paths) in [
            ("test-resources/testroot/src/*", &["bar/main.rs"][..]),
            ("test-resources/testroot/*/main.rs", &["bar/main.rs"]),
            ("test-resources/testroot/*/*", &["bar/src/main.rs", "bar/testchild/Cargo.toml"]),
            ("test-resources/*/src/*", &["bar/testroot/src/main.rs"]),
            ("test-resources/*/src/main.rs", &["bar/main.rs"]),
            ("test-resources/*/*/main.rs", &["bar/main.rs"]),
            ("test-resources/testroot/**/src/*", &["bar/src/main.rs", "bar/testchild/src/main.rs"]),
            ("test-resources/testroot/**/*.rs", &["bar/src/main.rs", "bar/testchild/src/main.rs"]),
        ] {
            let asset = UnresolvedAsset {
                source_path: PathBuf::from(glob),
                c: AssetCommon {
                    target_path: PathBuf::from("bar/"),
                    chmod: Some(0o644),
                    asset_kind: AssetKind::Any,
                    is_built: IsBuilt::SamePackage,
                },
            };
            let assets = asset
                .resolve(false)
                .unwrap()
                .into_iter()
                .map(|asset| asset.c.target_path.to_string_lossy().to_string())
                .collect::<Vec<_>>();
            if assets != paths {
                panic!("Glob: `{glob}`:\n  Expected: {paths:?}\n       Got: {assets:?}");
            }
        }
    }

    /// Tests that getting the debug filename from a path returns the same path
    /// with ".debug" appended
    #[test]
    fn test_debug_filename() {
        let path = Path::new("/my/test/file");
        assert_eq!(debug_filename(path), Path::new("/my/test/file.debug"));
    }

    /// Tests that getting the debug target for an Asset that `is_built` returns
    /// the path "/usr/lib/debug/<path-to-target>.debug"
    #[test]
    fn test_debug_target_ok() {
        let a = Asset::new(
            AssetSource::Path(PathBuf::from("target/release/bar")),
            PathBuf::from("/usr/bin/baz/"),
            Some(0o644),
            IsBuilt::SamePackage,
            AssetKind::Any,
        );
        let debug_target = a.c.default_debug_target_path("usr/lib".as_ref());
        assert_eq!(debug_target, Path::new("/usr/lib/debug/usr/bin/baz/bar.debug"));
    }

    /// Tests that getting the debug target for an Asset that `is_built` and that
    /// has a relative path target returns the path "/usr/lib/debug/<path-to-target>.debug"
    #[test]
    fn test_debug_target_ok_relative() {
        let a = Asset::new(
            AssetSource::Path(PathBuf::from("target/release/bar")),
            PathBuf::from("baz/"),
            Some(0o644),
            IsBuilt::Workspace,
            AssetKind::Any,
        );
        let debug_target = a.c.default_debug_target_path("usr/lib".as_ref());
        assert_eq!(debug_target, Path::new("/usr/lib/debug/baz/bar.debug"));
    }

    fn to_canon_static_str(s: &str) -> &'static str {
        let cwd = std::env::current_dir().unwrap();
        let abs_path = cwd.join(s);
        let abs_path_string = abs_path.to_string_lossy().into_owned();
        Box::leak(abs_path_string.into_boxed_str())
    }

    #[test]
    fn add_systemd_assets_with_no_config_does_nothing() {
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_progress().return_const(());

        // supply a systemd unit file as if it were available on disk
        let _g = add_test_fs_paths(&[to_canon_static_str("cargo-deb.service")]);

        let (_config, mut package_debs) = BuildEnvironment::from_manifest(BuildOptions {
            manifest_path: Some(Path::new("Cargo.toml")),
            debug: DebugSymbolOptions {
                #[cfg(feature = "default_enable_dbgsym")]
                generate_dbgsym_package: Some(false),
                #[cfg(feature = "default_enable_separate_debug_symbols")]
                separate_debug_symbols: Some(false),
                ..Default::default()
            },
            ..Default::default()
        }, &mock_listener).unwrap();
        let package_deb = package_debs.pop().unwrap();

        let num_unit_assets = package_deb.assets.resolved.iter()
            .filter(|a| a.c.target_path.starts_with("usr/lib/systemd/system/"))
            .count();

        assert_eq!(0, num_unit_assets);
    }

    #[test]
    fn add_systemd_assets_with_config_adds_unit_assets() {
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_progress().return_const(());

        // supply a systemd unit file as if it were available on disk
        let _g = add_test_fs_paths(&[to_canon_static_str("cargo-deb.service")]);

        let (_config, mut package_debs) = BuildEnvironment::from_manifest(BuildOptions {
            manifest_path: Some(Path::new("Cargo.toml")),
            debug: DebugSymbolOptions {
                #[cfg(feature = "default_enable_dbgsym")]
                generate_dbgsym_package: Some(false),
                #[cfg(feature = "default_enable_separate_debug_symbols")]
                separate_debug_symbols: Some(false),
                ..Default::default()
            },
            overrides: DebConfigOverrides {
                systemd_units: Some(vec![SystemdUnitsConfig::default()]),
                maintainer_scripts_rel_path: Some(PathBuf::new()),
                ..Default::default()
            },
            ..Default::default()
        }, &mock_listener).unwrap();
        let package_deb = package_debs.pop().unwrap();

        let num_unit_assets = package_deb.assets.resolved
            .iter()
            .filter(|a| a.c.target_path.starts_with("usr/lib/systemd/system/"))
            .count();

        assert_eq!(1, num_unit_assets);
    }
}
