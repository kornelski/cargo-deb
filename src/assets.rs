use crate::util::compress::gzipped;
use crate::config::is_glob_pattern;
use crate::config::Config;
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::util::read_file_to_bytes;
use std::borrow::Cow;
use std::env::consts::DLL_SUFFIX;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum AssetSource {
    /// Copy file from the path (and strip binary if needed).
    Path(PathBuf),
    /// A symlink existing in the file system
    Symlink(PathBuf),
    /// Write data to destination as-is.
    Data(Vec<u8>),
}

impl AssetSource {
    /// Symlink must exist on disk to be preserved
    #[must_use]
    pub fn from_path(path: impl Into<PathBuf>, preserve_existing_symlink: bool) -> Self {
        let path = path.into();
        if preserve_existing_symlink || !path.exists() { // !exists means a symlink to bogus path
            if let Ok(md) = fs::symlink_metadata(&path) {
                if md.is_symlink() {
                    return Self::Symlink(path);
                }
            }
        }
        Self::Path(path)
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            AssetSource::Symlink(ref p) |
            AssetSource::Path(ref p) => Some(p),
            AssetSource::Data(_) => None,
        }
    }

    #[must_use]
    pub fn into_path(self) -> Option<PathBuf> {
        match self {
            AssetSource::Symlink(p) |
            AssetSource::Path(p) => Some(p),
            AssetSource::Data(_) => None,
        }
    }

    #[must_use]
    pub fn archive_as_symlink_only(&self) -> bool {
        matches!(self, AssetSource::Symlink(_))
    }

    #[must_use]
    pub fn file_size(&self) -> Option<u64> {
        match *self {
            // FIXME: may not be accurate if the executable is not stripped yet?
            AssetSource::Path(ref p) => fs::metadata(p).ok().map(|m| m.len()),
            AssetSource::Data(ref d) => Some(d.len() as u64),
            AssetSource::Symlink(_) => None,
        }
    }

    pub fn data(&self) -> CDResult<Cow<'_, [u8]>> {
        Ok(match self {
            AssetSource::Path(p) => {
                let data = read_file_to_bytes(p)
                    .map_err(|e| CargoDebError::IoFile("unable to read asset to add to archive", e, p.clone()))?;
                Cow::Owned(data)
            },
            AssetSource::Data(d) => Cow::Borrowed(d),
            AssetSource::Symlink(_) => return Err(CargoDebError::Str("Symlink unexpectedly used to read file data")),
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Assets {
    pub unresolved: Vec<UnresolvedAsset>,
    pub resolved: Vec<Asset>,
}

impl Assets {
    pub(crate) fn new() -> Assets {
        Assets {
            unresolved: vec![],
            resolved: vec![],
        }
    }

    pub(crate) fn with_resolved_assets(assets: Vec<Asset>) -> Assets {
        Assets {
            unresolved: vec![],
            resolved: assets,
        }
    }

    pub(crate) fn with_unresolved_assets(assets: Vec<UnresolvedAsset>) -> Assets {
        Assets {
            unresolved: assets,
            resolved: vec![],
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.unresolved.is_empty() && self.resolved.is_empty()
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IsBuilt {
    No,
    SamePackage,
    /// needs --workspace to build
    Workspace,
}

#[derive(Debug, Clone)]
pub struct UnresolvedAsset {
    pub source_path: PathBuf,
    pub c: AssetCommon,
}

impl UnresolvedAsset {
    pub(crate) fn new(source_path: PathBuf, target_path: PathBuf, chmod: u32, is_built: IsBuilt, is_example: bool) -> Self {
        Self {
            source_path,
            c: AssetCommon { target_path, chmod, is_example, is_built },
        }
    }

    /// Convert `source_path` (with glob or dir) to actual path
    pub fn resolve(self, preserve_symlinks: bool) -> CDResult<Vec<Asset>> {
        let Self { source_path, c: AssetCommon { target_path, chmod, is_built, is_example } } = self;
        let source_prefix = is_glob_pattern(&source_path).then(|| {
            source_path.iter()
                .take_while(|&part| !is_glob_pattern(part.as_ref()))
                .collect::<PathBuf>()
        });
        let matched_assets = glob::glob(source_path.to_str().ok_or("utf8 path")?)?
            // Remove dirs from globs without throwing away errors
            .map(|entry| {
                let source_file = entry?;
                Ok(if source_file.is_dir() { None } else { Some(source_file) })
            })
            .filter_map(|res| {
                Some(res.transpose()?.map(|source_file| {
                    let target_file = if let Some(source_prefix) = &source_prefix {
                        target_path.join(source_file.strip_prefix(source_prefix).unwrap())
                    } else {
                        target_path.clone()
                    };
                    log::debug!("asset {} -> {} {} {:o}", source_file.display(), target_file.display(), if is_built != IsBuilt::No {"copy"} else {"build"}, chmod);
                    let asset = Asset::new(
                        AssetSource::from_path(source_file, preserve_symlinks),
                        target_file,
                        chmod,
                        is_built,
                        is_example,
                    );
                    if source_prefix.is_some() {
                        asset.processed("glob", None)
                    } else {
                        asset
                    }
                }))
            })
            .collect::<CDResult<Vec<_>>>()?;

        if matched_assets.is_empty() {
            return Err(CargoDebError::AssetFileNotFound(source_path));
        }
        Ok(matched_assets)
    }
}

#[derive(Debug, Clone)]
pub struct AssetCommon {
    pub target_path: PathBuf,
    pub chmod: u32,
    pub(crate) is_example: bool,
    is_built: IsBuilt,
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
    pub fn new(source: AssetSource, mut target_path: PathBuf, chmod: u32, is_built: IsBuilt, is_example: bool) -> Self {
        // is_dir() is only for paths that exist
        if target_path.to_string_lossy().ends_with('/') {
            let file_name = source.path().and_then(|p| p.file_name()).expect("source must be a file");
            target_path = target_path.join(file_name);
        }

        if target_path.is_absolute() || target_path.has_root() {
            target_path = target_path.strip_prefix("/").expect("no root dir").to_owned();
        }

        Self {
            source,
            processed_from: None,
            c: AssetCommon { target_path, chmod, is_example, is_built },
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
}

impl AssetCommon {
    pub(crate) fn is_executable(&self) -> bool {
        0 != self.chmod & 0o111
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
    pub(crate) fn default_debug_target_path(&self) -> PathBuf {
        // Turn an absolute path into one relative to "/"
        let relative = self.target_path.strip_prefix(Path::new("/"))
            .unwrap_or(self.target_path.as_path());

        // Prepend the debug location
        let debug_path = Path::new("/usr/lib/debug").join(relative);

        // Add `.debug` to the end of the filename
        debug_filename(&debug_path)
    }

    pub(crate) fn is_same_package(&self) -> bool {
        self.is_built != IsBuilt::SamePackage
    }
}

/// Adds `.debug` to the end of a path to a filename
///
fn debug_filename(path: &Path) -> PathBuf {
    let mut debug_filename = path.as_os_str().to_os_string();
    debug_filename.push(".debug");
    debug_filename.into()
}

pub(crate) fn is_dynamic_library_filename(path: &Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .map_or(false, |f| f.ends_with(DLL_SUFFIX))
}

/// Compress man pages and other assets per Debian Policy.
///
/// # References
///
/// <https://www.debian.org/doc/debian-policy/ch-docs.html>
/// <https://lintian.debian.org/tags/manpage-not-compressed.html>
pub fn compress_assets(config: &mut Config, listener: &dyn Listener) -> CDResult<()> {
    let mut indices_to_remove = Vec::new();
    let mut new_assets = Vec::new();

    fn needs_compression(path: &str) -> bool {
        !path.ends_with(".gz") &&
            (path.starts_with("usr/share/man/") ||
                (path.starts_with("usr/share/doc/") && (path.ends_with("/NEWS") || path.ends_with("/changelog"))) ||
                (path.starts_with("usr/share/info/") && path.ends_with(".info")))
    }

    for (idx, orig_asset) in config.deb.assets.resolved.iter().enumerate() {
        if !orig_asset.c.target_path.starts_with("usr") {
            continue;
        }
        let target_path_str = orig_asset.c.target_path.to_string_lossy();
        if needs_compression(&target_path_str) {
            debug_assert!(!orig_asset.c.is_built());

            let mut new_path = target_path_str.into_owned();
            new_path.push_str(".gz");
            listener.info(format!("Compressing '{new_path}'"));
            new_assets.push(Asset::new(
                crate::assets::AssetSource::Data(gzipped(&orig_asset.source.data()?)?),
                new_path.into(),
                orig_asset.c.chmod,
                IsBuilt::No,
                false,
            ).processed("compressed",
                orig_asset.source.path().unwrap_or(&orig_asset.c.target_path).to_path_buf()
            ));
            indices_to_remove.push(idx);
        }
    }

    for idx in indices_to_remove.iter().rev() {
        config.deb.assets.resolved.swap_remove(*idx);
    }

    config.deb.assets.resolved.append(&mut new_assets);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::manifest::SystemdUnitsConfig;
    use crate::util::tests::add_test_fs_paths;


    #[test]
    fn assets() {
        let a = Asset::new(
            AssetSource::Path(PathBuf::from("target/release/bar")),
            PathBuf::from("baz/"),
            0o644,
            IsBuilt::SamePackage,
            false,
        );
        assert_eq!("baz/bar", a.c.target_path.to_str().unwrap());
        assert!(a.c.is_built != IsBuilt::No);

        let a = Asset::new(
            AssetSource::Path(PathBuf::from("foo/bar")),
            PathBuf::from("/baz/quz"),
            0o644,
            IsBuilt::No,
            false,
        );
        assert_eq!("baz/quz", a.c.target_path.to_str().unwrap());
        assert!(a.c.is_built == IsBuilt::No);
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
            0o644,
            IsBuilt::SamePackage,
            false,
        );
        let debug_target = a.c.default_debug_target_path();
        assert_eq!(debug_target, Path::new("/usr/lib/debug/usr/bin/baz/bar.debug"));
    }

    /// Tests that getting the debug target for an Asset that `is_built` and that
    /// has a relative path target returns the path "/usr/lib/debug/<path-to-target>.debug"
    #[test]
    fn test_debug_target_ok_relative() {
        let a = Asset::new(
            AssetSource::Path(PathBuf::from("target/release/bar")),
            PathBuf::from("baz/"),
            0o644,
            IsBuilt::Workspace,
            false,
        );
        let debug_target = a.c.default_debug_target_path();
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
        mock_listener.expect_info().return_const(());

        // supply a systemd unit file as if it were available on disk
        let _g = add_test_fs_paths(&[to_canon_static_str("cargo-deb.service")]);

        let config = Config::from_manifest(Some(Path::new("Cargo.toml")), None, None, None, None, None, None, &mock_listener, "release", None, None).unwrap();

        let num_unit_assets = config.deb.assets.resolved.iter()
            .filter(|a| a.c.target_path.starts_with("lib/systemd/system/"))
            .count();

        assert_eq!(0, num_unit_assets);
    }

    #[test]
    fn add_systemd_assets_with_config_adds_unit_assets() {
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_info().return_const(());

        // supply a systemd unit file as if it were available on disk
        let _g = add_test_fs_paths(&[to_canon_static_str("cargo-deb.service")]);

        let mut config = Config::from_manifest(Some(Path::new("Cargo.toml")), None, None, None, None, None, None, &mock_listener, "release", None, None).unwrap();

        config.deb.systemd_units.get_or_insert(vec![SystemdUnitsConfig::default()]);
        config.deb.maintainer_scripts.get_or_insert(PathBuf::new());

        config.add_systemd_assets().unwrap();

        let num_unit_assets = config.deb.assets.resolved
            .iter()
            .filter(|a| a.c.target_path.starts_with("lib/systemd/system/"))
            .count();

        assert_eq!(1, num_unit_assets);
    }
}
