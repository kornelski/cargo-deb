use crate::assets::RawAsset;
use crate::error::{CDResult, CargoDebError};
use crate::CargoLockingFlags;
use cargo_toml::DebugSetting;
use log::debug;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Configuration settings for the `systemd_units` functionality.
///
/// `unit_scripts`: (optional) relative path to a directory containing correctly
/// named systemd unit files. See `dh_lib::pkgfile()` and `dh_installsystemd.rs`
/// for more details on file naming. If not supplied, defaults to the
/// `maintainer_scripts` directory.
///
/// `unit_name`: (optjonal) in cases where the `unit_scripts` directory contains
/// multiple units, only process those matching this unit name.
///
/// For details on the other options please see `dh_installsystemd::Options`.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct SystemdUnitsConfig {
    pub unit_scripts: Option<PathBuf>,
    pub unit_name: Option<String>,
    pub enable: Option<bool>,
    pub start: Option<bool>,
    pub restart_after_upgrade: Option<bool>,
    pub stop_on_upgrade: Option<bool>,
}

pub(crate) fn manifest_debug_flag(manifest: &cargo_toml::Manifest<CargoPackageMetadata>, selected_profile: &str) -> Option<bool> {
    let profile = if selected_profile == "release" {
        manifest.profile.release.as_ref()?
    } else {
        manifest.profile.custom.get(selected_profile)?
    };
    Some(*profile.debug.as_ref()? != DebugSetting::None)
}

/// Debian-compatible version of the semver version
pub(crate) fn manifest_version_string<'a>(package: &'a cargo_toml::Package<CargoPackageMetadata>, revision: Option<&str>) -> Cow<'a, str> {
    let mut version = Cow::Borrowed(package.version());

    // Make debian's version ordering (newer versions) more compatible with semver's.
    // Keep "semver-1" and "semver-xxx" as-is (assuming these are irrelevant, or debian revision already),
    // but change "semver-beta.1" to "semver~beta.1"
    if let Some((semver_main, semver_pre)) = version.split_once('-') {
        let pre_ascii = semver_pre.as_bytes();
        if pre_ascii.iter().any(|c| !c.is_ascii_digit()) && pre_ascii.iter().any(u8::is_ascii_digit) {
            version = Cow::Owned(format!("{semver_main}~{semver_pre}"));
        }
    }

    let revision = revision.unwrap_or("1");
    if !revision.is_empty() && revision != "0" {
        let v = version.to_mut();
        v.push('-');
        v.push_str(revision);
    }
    version
}

#[derive(Clone, Debug, Deserialize, Default)]
pub(crate) struct CargoPackageMetadata {
    pub deb: Option<CargoDeb>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum LicenseFile {
    String(String),
    Vec(Vec<String>),
}

#[derive(Deserialize, Clone, Debug)]
#[serde(untagged)]
pub(crate) enum SystemUnitsSingleOrMultiple {
    Single(SystemdUnitsConfig),
    Multi(Vec<SystemdUnitsConfig>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum DependencyList {
    String(String),
    Vec(Vec<String>),
}

impl DependencyList {
    pub(crate) fn into_depends_string(self) -> String {
        match self {
            Self::String(s) => s,
            Self::Vec(vals) => vals.join(", "),
        }
    }
}

/// Type-alias for list of assets
///
pub(crate) type AssetList = Vec<RawAsset>;

/// Type-alias for a merge map,
///
pub(crate) type MergeMap<'a> = BTreeMap<&'a PathBuf, (&'a PathBuf, u32)>;

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum CargoDebAssetArrayOrTable {
    Table(CargoDebAsset),
    Array([String; 3]),
    Invalid(toml::Value),
}

#[derive(Clone, Debug, Deserialize, Default)]
pub(crate) struct CargoDebAsset {
    pub source: String,
    pub dest: String,
    pub mode: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct CargoDeb {
    pub name: Option<String>,
    pub maintainer: Option<String>,
    pub copyright: Option<String>,
    pub license_file: Option<LicenseFile>,
    pub changelog: Option<String>,
    pub depends: Option<DependencyList>,
    pub pre_depends: Option<DependencyList>,
    pub recommends: Option<DependencyList>,
    pub suggests: Option<DependencyList>,
    pub enhances: Option<DependencyList>,
    pub conflicts: Option<DependencyList>,
    pub breaks: Option<DependencyList>,
    pub replaces: Option<DependencyList>,
    pub provides: Option<DependencyList>,
    pub extended_description: Option<String>,
    pub extended_description_file: Option<String>,
    pub section: Option<String>,
    pub priority: Option<String>,
    pub revision: Option<String>,
    pub conf_files: Option<Vec<String>>,
    pub assets: Option<AssetList>,
    pub merge_assets: Option<MergeAssets>,
    pub triggers_file: Option<String>,
    pub maintainer_scripts: Option<String>,
    pub features: Option<Vec<String>>,
    pub default_features: Option<bool>,
    pub separate_debug_symbols: Option<bool>,
    pub compress_debug_symbols: Option<bool>,
    pub preserve_symlinks: Option<bool>,
    pub systemd_units: Option<SystemUnitsSingleOrMultiple>,
    pub variants: Option<HashMap<String, CargoDeb>>,
}

/// Struct containing merge configuration
///
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct MergeAssets {
    /// Merge assets by appending this list,
    ///
    pub append: Option<AssetList>,
    /// Merge assets using the src as the key,
    ///
    pub by: Option<MergeByKey>,
}

/// Enumeration of merge by key strategies
///
#[derive(Clone, Debug, Deserialize)]
pub(crate) enum MergeByKey {
    #[serde(rename = "src")]
    Src(AssetList),
    #[serde(rename = "dest")]
    Dest(AssetList),
}

impl MergeByKey {
    /// Merges w/ a parent asset list
    ///
    fn merge(self, parent: &AssetList) -> AssetList {
        let merge_map = {
            parent.iter().fold(BTreeMap::new(), |parent, asset| {
                self.prep_parent_item(parent, asset)
            })
        };

        self.merge_with(merge_map)
    }

    /// Folds the parent asset into a merge-map preparing to prepare for a merge,
    ///
    fn prep_parent_item<'a>(&'a self, mut parent: MergeMap<'a>, RawAsset { source_path: src,target_path: dest, chmod: perm }: &'a RawAsset) -> MergeMap<'a> {
        match &self {
            Self::Src(_) => {
                parent.insert(src, (dest, *perm));
            },
            Self::Dest(_) => {
                parent.insert(dest, (src, *perm));
            },
        }
        parent
    }

    /// Merges w/ a parent merge map and returns the resulting asset list,
    ///
    fn merge_with(&self, parent: MergeMap<'_>) -> AssetList {
        match self {
            Self::Src(assets) => assets.iter()
                .fold(parent, |mut acc, RawAsset { source_path: src,target_path: dest, chmod: perm }| {
                    if let Some((replaced_dest, replaced_perm)) = acc.insert(src, (dest, *perm)) {
                        debug!("Replacing {:?} w/ {:?}", (replaced_dest, replaced_perm), (dest, perm));
                    }
                    acc
                })
                .into_iter()
                .map(|(src, (dest, perm))| RawAsset { source_path: src.clone(), target_path: dest.clone(), chmod: perm })
                .collect(),
            Self::Dest(assets) => assets.iter()
                .fold(parent, |mut acc, RawAsset { source_path: src, target_path: dest, chmod: perm }| {
                    if let Some((replaced_src, replaced_perm)) = acc.insert(dest, (src, *perm)) {
                        debug!("Replacing {:?} w/ {:?}", (replaced_src, replaced_perm), (src, perm));
                    }
                    acc
                })
                .into_iter()
                .map(|(dest, (src, perm))| RawAsset { source_path: src.clone(), target_path: dest.clone(), chmod: perm })
                .collect(),
        }
    }
}

impl CargoDeb {
    /// Inherit unset fields from parent,
    ///
    /// **Note**: For backwards compat, if `merge_assets` is set, this will apply **after** the variant has overridden the assets.
    ///
    pub(crate) fn inherit_from(self, parent: Self) -> Self {
        let mut assets = self.assets.or(parent.assets);

        if let (Some(merge_assets), Some(old_assets)) = (self.merge_assets, assets.as_mut()) {
            if let Some(mut append) = merge_assets.append {
                old_assets.append(&mut append);
            }

            if let Some(strategy) = merge_assets.by {
                assets = Some(strategy.merge(old_assets));
            }
        }

        Self {
            name: self.name.or(parent.name),
            maintainer: self.maintainer.or(parent.maintainer),
            copyright: self.copyright.or(parent.copyright),
            license_file: self.license_file.or(parent.license_file),
            changelog: self.changelog.or(parent.changelog),
            depends: self.depends.or(parent.depends),
            pre_depends: self.pre_depends.or(parent.pre_depends),
            recommends: self.recommends.or(parent.recommends),
            suggests: self.suggests.or(parent.suggests),
            enhances: self.enhances.or(parent.enhances),
            conflicts: self.conflicts.or(parent.conflicts),
            breaks: self.breaks.or(parent.breaks),
            replaces: self.replaces.or(parent.replaces),
            provides: self.provides.or(parent.provides),
            extended_description: self.extended_description.or(parent.extended_description),
            extended_description_file: self.extended_description_file.or(parent.extended_description_file),
            section: self.section.or(parent.section),
            priority: self.priority.or(parent.priority),
            revision: self.revision.or(parent.revision),
            conf_files: self.conf_files.or(parent.conf_files),
            assets,
            merge_assets: None,
            triggers_file: self.triggers_file.or(parent.triggers_file),
            maintainer_scripts: self.maintainer_scripts.or(parent.maintainer_scripts),
            features: self.features.or(parent.features),
            default_features: self.default_features.or(parent.default_features),
            separate_debug_symbols: self.separate_debug_symbols.or(parent.separate_debug_symbols),
            compress_debug_symbols: self.compress_debug_symbols.or(parent.compress_debug_symbols),
            preserve_symlinks: self.preserve_symlinks.or(parent.preserve_symlinks),
            systemd_units: self.systemd_units.or(parent.systemd_units),
            variants: self.variants.or(parent.variants),
        }
    }
}

#[derive(Deserialize)]
struct CargoMetadata {
    pub packages: Vec<CargoMetadataPackage>,
    pub resolve: CargoMetadataResolve,
    #[serde(default)]
    pub workspace_members: Vec<String>,
    #[serde(default)]
    pub workspace_default_members: Vec<String>,
    pub target_directory: String,
    #[serde(default)]
    pub workspace_root: String,
}

#[derive(Deserialize)]
struct CargoMetadataResolve {
    pub root: Option<String>,
}

#[derive(Deserialize)]
struct CargoMetadataPackage {
    pub id: String,
    pub name: String,
    pub targets: Vec<CargoMetadataTarget>,
    pub manifest_path: PathBuf,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CargoMetadataTarget {
    pub name: String,
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub src_path: PathBuf,
}

pub(crate) struct ManifestFound {
    pub build_targets: Vec<CargoMetadataTarget>,
    pub manifest_path: PathBuf,
    pub root_manifest: Option<cargo_toml::Manifest<CargoPackageMetadata>>,
    pub target_dir: PathBuf,
    pub manifest: cargo_toml::Manifest<CargoPackageMetadata>,
    /// Cargo is sensitive to the current directory it's been invoked from - relative `CARGO_TARGET_DIR` and `.cargo` dir discovery
    /// can significantly affect the build, and are disconnected from locations of the manifest and the workspace!
    pub cargo_run_current_dir: PathBuf,
}

fn parse_metadata(mut metadata: CargoMetadata, selected_package_name: Option<&str>) -> Result<(CargoMetadataPackage, PathBuf, PathBuf), CargoDebError> {
    let available_package_names = || {
        metadata.packages.iter()
            .filter(|p| metadata.workspace_members.iter().any(|w| w == &p.id))
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>().join(", ")
    };
    let target_package_pos = if let Some(name) = selected_package_name {
        metadata.packages.iter().position(|p| p.name == name)
            .ok_or_else(|| CargoDebError::PackageNotFoundInWorkspace(name.into(), available_package_names()))
    } else {
        metadata.workspace_default_members.first()
            .filter(|_| metadata.workspace_default_members.len() == 1)
            .or(metadata.resolve.root.as_ref())
            .and_then(|root_id| metadata.packages.iter().position(move |p| &p.id == root_id))
            .ok_or_else(|| CargoDebError::NoRootFoundInWorkspace(available_package_names()))
    }?;
    Ok((metadata.packages.swap_remove(target_package_pos), metadata.target_directory.into(), metadata.workspace_root.into()))
}

pub(crate) fn cargo_metadata(root_manifest_path: Option<&Path>, selected_package_name: Option<&str>, cargo_locking_flags: CargoLockingFlags) -> Result<ManifestFound, CargoDebError> {
    let (metadata, cargo_run_current_dir) = run_cargo_metadata(root_manifest_path, cargo_locking_flags)?;
    let (target_package, target_dir, workspace_root) = parse_metadata(metadata, selected_package_name)?;

    let workspace_root_manifest_path = workspace_root.join("Cargo.toml");
    let root_manifest = cargo_toml::Manifest::<CargoPackageMetadata>::from_path_with_metadata(workspace_root_manifest_path).ok();
    let manifest_path = Path::new(&target_package.manifest_path);
    let manifest_bytes = fs::read(manifest_path).map_err(|e| CargoDebError::IoFile("unable to read manifest", e, manifest_path.to_owned()))?;
    let mut manifest = cargo_toml::Manifest::<CargoPackageMetadata>::from_slice_with_metadata(&manifest_bytes)
        .map_err(|e| CargoDebError::TomlParsing(e, manifest_path.into()))?;
    let ws_root = root_manifest.as_ref().map(|ws| (ws, workspace_root.as_path()));
    manifest.complete_from_path_and_workspace(manifest_path, ws_root)
        .map_err(move |e| CargoDebError::TomlParsing(e, manifest_path.to_path_buf()))?;

    Ok(ManifestFound {
        manifest_path: target_package.manifest_path,
        build_targets: target_package.targets,
        root_manifest,
        target_dir,
        manifest,
        cargo_run_current_dir,
    })
}

/// Returns the workspace metadata based on the `Cargo.toml` that we want to build,
/// and directory that paths may be relative to
fn run_cargo_metadata(manifest_rel_path: Option<&Path>, cargo_locking_flags: CargoLockingFlags) -> CDResult<(CargoMetadata, PathBuf)> {
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--format-version=1"]);
    cmd.args(cargo_locking_flags.flags());

    let current_dir = if let Some(path) = manifest_rel_path {
        // cargo will read ./.config relative to the current dir,
        // so --manifest-path of another dir can end up finding a wrong config
        // if the current dir isn't set to match.
        let tmp;
        let manifest_abs_path = if path.is_absolute() { path } else {
            tmp = path.canonicalize().map_err(|e| CargoDebError::IoFile("bad manifest path", e, path.into()))?;
            &*tmp
        };
        cmd.args(["--manifest-path".as_ref(), manifest_abs_path.as_os_str()]);

        manifest_abs_path.parent().ok_or("bad manifest path")?.to_owned()
    } else {
        std::env::current_dir()?
    };
    cmd.current_dir(&current_dir);

    let output = cmd.output()
        .map_err(|e| CargoDebError::CommandFailed(e, "cargo (is it in your PATH?)"))?;
    if !output.status.success() {
        return Err(CargoDebError::CommandError("cargo", "metadata".to_owned(), output.stderr));
    }

    let metadata = serde_json::from_slice(&output.stdout)?;
    Ok((metadata, current_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_assets() {
        // Test merging assets by dest
        fn create_test_asset(src: impl Into<PathBuf>, target_path: impl Into<PathBuf>, perm: u32) -> RawAsset {
            RawAsset {
                source_path: src.into(), target_path: target_path.into(), chmod: perm
            }
        }

        // Test merging assets by dest
        let original_asset = create_test_asset(
            "lib/test/empty.txt",
            "/opt/test/empty.txt",
            0o777
        );

        let merge_asset = create_test_asset(
            "lib/test_variant/empty.txt",
            "/opt/test/empty.txt",
            0o655,
        );

        let parent = CargoDeb { assets: Some(vec![ original_asset ]), .. Default::default() };
        let variant = CargoDeb { merge_assets: Some(MergeAssets { append: None, by: Some(MergeByKey::Dest(vec![ merge_asset ])) }), .. Default::default() };

        let merged = variant.inherit_from(parent);
        let mut merged = merged.assets.expect("should have assets");
        let merged_asset = merged.pop().expect("should have an asset");
        assert_eq!("lib/test_variant/empty.txt", merged_asset.source_path.as_os_str(), "should have merged the source location");
        assert_eq!("/opt/test/empty.txt", merged_asset.target_path.as_os_str(), "should preserve dest location");
        assert_eq!(0o655, merged_asset.chmod, "should have merged the dest location");

        // Test merging assets by src
        let original_asset = create_test_asset(
            "lib/test/empty.txt",
            "/opt/test/empty.txt",
            0o777
        );

        let merge_asset = create_test_asset(
            "lib/test/empty.txt",
            "/opt/test_variant/empty.txt",
            0o655,
        );

        let parent = CargoDeb { assets: Some(vec![ original_asset ]), .. Default::default() };
        let variant = CargoDeb { merge_assets: Some(MergeAssets { append: None, by: Some(MergeByKey::Src(vec![ merge_asset ])) }), .. Default::default() };

        let merged = variant.inherit_from(parent);
        let mut merged = merged.assets.expect("should have assets");
        let merged_asset = merged.pop().expect("should have an asset");
        assert_eq!("lib/test/empty.txt", merged_asset.source_path.as_os_str(), "should have merged the source location");
        assert_eq!("/opt/test_variant/empty.txt", merged_asset.target_path.as_os_str(), "should preserve dest location");
        assert_eq!(0o655, merged_asset.chmod, "should have merged the dest location");

        // Test merging assets by appending
        let original_asset = create_test_asset(
            "lib/test/empty.txt",
            "/opt/test/empty.txt",
            0o777
        );

        let merge_asset = create_test_asset(
            "lib/test/empty.txt",
            "/opt/test_variant/empty.txt",
            0o655,
        );
        
        let parent = CargoDeb { assets: Some(vec![ original_asset ]), .. Default::default() };
        let variant = CargoDeb { merge_assets: Some(MergeAssets { append: Some(vec![merge_asset]), by: None }), .. Default::default() };
        
        let merged = variant.inherit_from(parent);
        let mut merged = merged.assets.expect("should have assets");

        let merged_asset = merged.pop().expect("should have an asset");
        assert_eq!("lib/test/empty.txt", merged_asset.source_path.as_os_str(), "should have merged the source location");
        assert_eq!("/opt/test_variant/empty.txt", merged_asset.target_path.as_os_str(), "should preserve dest location");
        assert_eq!(0o655, merged_asset.chmod, "should have merged the dest location");

        let merged_asset = merged.pop().expect("should have an asset");
        assert_eq!("lib/test/empty.txt", merged_asset.source_path.as_os_str(), "should have merged the source location");
        assert_eq!("/opt/test/empty.txt", merged_asset.target_path.as_os_str(), "should preserve dest location");
        assert_eq!(0o777, merged_asset.chmod, "should have merged the dest location");

        // Test backwards compatibility for variants that have set assets
        let original_asset = create_test_asset(
            "lib/test/empty.txt",
            "/opt/test/empty.txt",
            0o777,
        );

        let merge_asset = create_test_asset(
            "lib/test_variant/empty.txt",
            "/opt/test/empty.txt",
            0o655,
        );

        let additional_asset = create_test_asset(
            "lib/test/other-empty.txt",
            "/opt/test/other-empty.txt",
            0o655,
        );

        let parent = CargoDeb { assets: Some(vec![ original_asset ]), .. Default::default() };
        let variant = CargoDeb { merge_assets: Some(MergeAssets { append: None, by: Some(MergeByKey::Dest(vec![ merge_asset.clone() ])) }), assets: Some(vec![ merge_asset, additional_asset ]), .. Default::default() };

        let merged = variant.inherit_from(parent);
        let mut merged = merged.assets.expect("should have assets");
        let merged_asset = merged.remove(0);
        assert_eq!("lib/test_variant/empty.txt", merged_asset.source_path.as_os_str(), "should have merged the source location");
        assert_eq!("/opt/test/empty.txt", merged_asset.target_path.as_os_str(), "should preserve dest location");
        assert_eq!(0o655, merged_asset.chmod, "should have merged the dest location");

        let additional_asset = merged.remove(0);
        assert_eq!("lib/test/other-empty.txt", additional_asset.source_path.as_os_str(), "should have merged the source location");
        assert_eq!("/opt/test/other-empty.txt", additional_asset.target_path.as_os_str(), "should preserve dest location");
        assert_eq!(0o655, additional_asset.chmod, "should have merged the dest location");
    }
}

#[test]
fn deb_ver() {
    let mut c = cargo_toml::Package::new("test", "1.2.3-1");
    assert_eq!("1.2.3-1-1", manifest_version_string(&c, None));
    assert_eq!("1.2.3-1-2", manifest_version_string(&c, Some("2")));
    assert_eq!("1.2.3-1", manifest_version_string(&c, Some("")));
    c.version = cargo_toml::Inheritable::Set("1.2.0-beta.3".into());
    assert_eq!("1.2.0~beta.3-1", manifest_version_string(&c, None));
    assert_eq!("1.2.0~beta.3-4", manifest_version_string(&c, Some("4")));
    assert_eq!("1.2.0~beta.3", manifest_version_string(&c, Some("")));
    c.version = cargo_toml::Inheritable::Set("1.2.0-new".into());
    assert_eq!("1.2.0-new-1", manifest_version_string(&c, None));
    assert_eq!("1.2.0-new-11", manifest_version_string(&c, Some("11")));
    assert_eq!("1.2.0-new", manifest_version_string(&c, Some("0")));
}
