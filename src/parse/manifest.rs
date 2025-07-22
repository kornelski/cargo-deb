use crate::assets::{RawAsset, RawAssetOrAuto};
use crate::config::BuildProfile;
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::CargoLockingFlags;
use cargo_toml::{DebugSetting, StripSetting};
use log::debug;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const USR_MERGE_DEFAULT: bool = false;

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
    pub usr_merge: Option<bool>,
}

#[derive(PartialEq, Copy, Clone, Debug)]
pub(crate) enum ManifestDebugFlags {
    /// Don't bother stripping again
    FullyStrippedByCargo,
    /// Explicitly doesn't want debug symbols
    SymbolsDisabled,
    SymbolsPackedExternally,
    SomeSymbolsAdded,
    FullSymbolsAdded,
    /// Not explicitly specified either way
    Default,
}

pub(crate) fn find_profile<'a>(manifest: &'a cargo_toml::Manifest<CargoPackageMetadata>, selected_profile: &str) -> Option<&'a cargo_toml::Profile> {
    if selected_profile == "release" {
        manifest.profile.release.as_ref()
    } else {
        manifest.profile.custom.get(selected_profile)
    }
}

fn from_toml_value<T: DeserializeOwned>(toml: &str) -> Option<T> {
    // support parsing `true` as bool, but other values as strings
    T::deserialize(toml::de::ValueDeserializer::new(toml)).ok().or_else(|| {
        T::deserialize(toml::de::ValueDeserializer::new(&format!("\"{toml}\"")))
            .inspect_err(|e| log::warn!("error parsing profile override: {toml}\n{e}")).ok()
    })
}

pub(crate) fn debug_flags(manifest_profile: Option<&cargo_toml::Profile>, profile_override: &BuildProfile) -> ManifestDebugFlags {
    let profile_uppercase = profile_override.profile_name().to_ascii_uppercase();
    let cargo_var = |name| {
        let name = format!("CARGO_PROFILE_{profile_uppercase}_{name}");
        std::env::var(&name).ok().inspect(|v| log::debug!("{name} = {v}"))
    };

    let strip = cargo_var("STRIP").and_then(|var| from_toml_value::<StripSetting>(&var))
        .or(manifest_profile.and_then(|p| p.strip.clone())).inspect(|v| log::debug!("strip={v:?}"));
    if strip == Some(StripSetting::Symbols) {
        return ManifestDebugFlags::FullyStrippedByCargo;
    }

    let debug = profile_override.override_debug.clone().inspect(|o| log::debug!("override={o}")).or_else(|| cargo_var("DEBUG"))
        .and_then(|var| from_toml_value::<DebugSetting>(&var))
        .or(manifest_profile.and_then(|p| p.debug.clone())).inspect(|v| log::debug!("debug={v:?}"));
    match debug {
        None => ManifestDebugFlags::Default,
        Some(DebugSetting::None) => ManifestDebugFlags::SymbolsDisabled,
        Some(_) if manifest_profile.and_then(|p| p.split_debuginfo.as_deref()).is_some_and(|p| p != "off") => ManifestDebugFlags::SymbolsPackedExternally,
        Some(DebugSetting::Full) if strip != Some(StripSetting::Debuginfo) => ManifestDebugFlags::FullSymbolsAdded,
        Some(_) => ManifestDebugFlags::SomeSymbolsAdded,
    }
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
pub(crate) type RawAssetList = Vec<RawAssetOrAuto>;

#[derive(Default)]
pub(crate) struct MergeMap<'a> {
    by_path: BTreeMap<&'a PathBuf, (&'a PathBuf, u32)>,
    has_auto: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum CargoDebAssetArrayOrTable {
    Table(CargoDebAsset),
    Array([String; 3]),
    Auto(String),
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
    pub assets: Option<RawAssetList>,
    pub merge_assets: Option<MergeAssets>,
    pub triggers_file: Option<String>,
    pub maintainer_scripts: Option<String>,
    pub features: Option<Vec<String>>,
    pub default_features: Option<bool>,
    pub separate_debug_symbols: Option<bool>,
    pub dbgsym: Option<bool>,
    pub compress_debug_symbols: Option<bool>,
    pub preserve_symlinks: Option<bool>,
    pub systemd_units: Option<SystemUnitsSingleOrMultiple>,
    pub variants: Option<HashMap<String, CargoDeb>>,

    /// Cargo build profile, defaults to `release`
    pub profile: Option<String>,
}

/// Struct containing merge configuration
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct MergeAssets {
    /// Merge assets by appending this list,
    pub append: Option<RawAssetList>,
    /// Merge assets using the src as the key,
    pub by: Option<MergeByKey>,
}

/// Enumeration of merge by key strategies
#[derive(Clone, Debug, Deserialize)]
pub(crate) enum MergeByKey {
    #[serde(rename = "src")]
    Src(RawAssetList),
    #[serde(rename = "dest")]
    Dest(RawAssetList),
}

impl MergeByKey {
    /// Merges w/ a parent asset list
    fn merge(self, parent: &RawAssetList) -> RawAssetList {
        let mut merge_map = MergeMap::default();
        for asset in parent {
            match asset {
                RawAssetOrAuto::Auto => { merge_map.has_auto = true; },
                RawAssetOrAuto::RawAsset(asset) => self.prep_parent_item(&mut merge_map, asset),
            }
        }

        self.merge_with(merge_map)
    }

    /// Folds the parent asset into a merge-map preparing to prepare for a merge,
    ///
    fn prep_parent_item<'a>(&'a self, merge_map: &mut MergeMap<'a>, RawAsset { source_path: src,target_path: dest, chmod: perm }: &'a RawAsset) {
        match &self {
            Self::Src(_) => {
                merge_map.by_path.insert(src, (dest, *perm));
            },
            Self::Dest(_) => {
                merge_map.by_path.insert(dest, (src, *perm));
            },
        }
    }

    /// Merges w/ a parent merge map and returns the resulting asset list,
    ///
    fn merge_with<'a>(&'a self, mut merge_map: MergeMap<'a>) -> RawAssetList {
        let (assets, merge_fn, combine_fn): (_, fn(&mut MergeMap<'a>, &'a RawAsset), fn(_) -> RawAsset) = match self {
            Self::Src(assets) => (
                assets,
                |parent, RawAsset { source_path: src, target_path: dest, chmod: perm }| {
                    if let Some((replaced_dest, replaced_perm)) = parent.by_path.insert(src, (dest, *perm)) {
                        debug!("Replacing {:?} w/ {:?}", (replaced_dest, replaced_perm), (dest, perm));
                    }
                },
                |(src, (dest, perm))| RawAsset { source_path: src, target_path: dest, chmod: perm },
            ),
            Self::Dest(assets) => (
                assets,
                |parent, RawAsset { source_path: src, target_path: dest, chmod: perm }| {
                    if let Some((replaced_src, replaced_perm)) = parent.by_path.insert(dest, (src, *perm)) {
                        debug!("Replacing {:?} w/ {:?}", (replaced_src, replaced_perm), (src, perm));
                    }
                },
                |(dest, (src, perm))| RawAsset { source_path: src, target_path: dest, chmod: perm },
            ),
        };

        for asset in assets {
            match asset {
                RawAssetOrAuto::RawAsset(asset) => {
                    merge_fn(&mut merge_map, asset);
                },
                RawAssetOrAuto::Auto => merge_map.has_auto = true,
            }
        }

        merge_map.by_path
            .into_iter()
            .map(|(path1, (path2, perm))| (path1.clone(), (path2.clone(), perm)))
            .map(combine_fn)
            .map(RawAssetOrAuto::RawAsset)
            .chain(merge_map.has_auto.then_some(RawAssetOrAuto::Auto))
            .collect()
    }
}

impl CargoDeb {
    /// Inherit unset fields from parent,
    ///
    /// **Note**: For backwards compat, if `merge_assets` is set, this will apply **after** the variant has overridden the assets.
    ///
    pub(crate) fn inherit_from(self, parent: Self, listener: &dyn Listener) -> Self {
        let mut assets = self.assets.or(parent.assets);

        if let Some(merge_assets) = self.merge_assets {
            let old_assets = assets.get_or_insert_with(|| {
                listener.warning(format!("variant has merge-assets, but not assets to merge"));
                vec![]
            });
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
            dbgsym: self.dbgsym.or(parent.dbgsym),
            separate_debug_symbols: self.separate_debug_symbols.or(parent.separate_debug_symbols),
            compress_debug_symbols: self.compress_debug_symbols.or(parent.compress_debug_symbols),
            preserve_symlinks: self.preserve_symlinks.or(parent.preserve_symlinks),
            systemd_units: self.systemd_units.or(parent.systemd_units),
            variants: self.variants.or(parent.variants),
            profile: self.profile.or(parent.profile),
        }
    }
}

#[derive(Deserialize)]
struct CargoMetadata {
    pub packages: Vec<CargoMetadataPackage>,
    #[serde(default)]
    pub workspace_members: Vec<String>,
    #[serde(default)]
    pub workspace_default_members: Vec<String>,
    pub target_directory: String,
    #[serde(default)]
    pub workspace_root: String,
}

#[derive(Deserialize)]
struct CargoMetadataPackage {
    pub id: String,
    pub name: String,
    pub targets: Vec<CargoMetadataTarget>,
    pub manifest_path: PathBuf,
    pub metadata: Option<toml::Value>,
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
    pub workspace_root_manifest_path: PathBuf,
    pub root_manifest: Option<cargo_toml::Manifest<CargoPackageMetadata>>,
    pub target_dir: PathBuf,
    pub manifest: cargo_toml::Manifest<CargoPackageMetadata>,
}

fn parse_metadata(mut metadata: CargoMetadata, selected_package_name: Option<&str>) -> Result<(CargoMetadataPackage, PathBuf, PathBuf), CargoDebError> {
    let available_package_names = || {
        metadata.packages.iter()
            .filter(|p| metadata.workspace_members.iter().any(|w| w == &p.id))
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>().join(", ")
    };
    let target_package_pos = if let Some(name) = selected_package_name {
        let name_no_ver = name.split('@').next().unwrap_or_default();
        metadata.packages.iter().position(|p| p.name == name_no_ver)
            .ok_or_else(|| CargoDebError::PackageNotFoundInWorkspace(name.into(), available_package_names()))
    } else {
        pick_default_package_from_workspace(&metadata)
            .ok_or_else(|| CargoDebError::NoRootFoundInWorkspace(available_package_names()))
    }?;
    Ok((metadata.packages.swap_remove(target_package_pos), metadata.target_directory.into(), metadata.workspace_root.into()))
}

fn pick_default_package_from_workspace(metadata: &CargoMetadata) -> Option<usize> {
    // ignore default_members if there are multiple due to ambiguity
    if let [root_id] = metadata.workspace_default_members.as_slice() {
        if let Some(pos) = metadata.packages.iter().position(move |p| &p.id == root_id) {
            return Some(pos);
        }
    }

    // if the root manifest is a package, use it
    let root_manifest_path = Path::new(&metadata.workspace_root).join("Cargo.toml");
    if let Some(pos) = metadata.packages.iter().position(move |p| p.manifest_path == root_manifest_path) {
        return Some(pos);
    }

    // find (active) package with an explicit cargo-deb metadata
    let default_members = if !metadata.workspace_default_members.is_empty() {
        &metadata.workspace_default_members[..]
    } else {
        &metadata.workspace_members
    };
    let mut packages_with_deb_meta = metadata.packages.iter().enumerate().filter_map(|(i, package)| {
        if !package.metadata.as_ref()?.as_table()?.contains_key("deb") {
            return None;
        }
        default_members.contains(&package.id).then_some(i)
    });
    let expected_single_id = packages_with_deb_meta.next()?;
    packages_with_deb_meta.next().is_none().then_some(expected_single_id)
}

fn parse_manifest_only(manifest_path: &Path) -> Result<cargo_toml::Manifest<CargoPackageMetadata>, CargoDebError> {
    let manifest_bytes = fs::read(manifest_path)
        .map_err(|e| CargoDebError::IoFile("unable to read manifest", e, manifest_path.to_owned()))?;

    cargo_toml::Manifest::<CargoPackageMetadata>::from_slice_with_metadata(&manifest_bytes)
            .map_err(|e| CargoDebError::TomlParsing(e, manifest_path.into()))
}

pub(crate) fn cargo_metadata(initial_manifest_path: Option<&Path>, selected_package_name: Option<&str>, cargo_locking_flags: CargoLockingFlags) -> Result<ManifestFound, CargoDebError> {
    let metadata = run_cargo_metadata(initial_manifest_path, cargo_locking_flags)?;
    let (target_package, target_dir, workspace_root) = parse_metadata(metadata, selected_package_name)?;

    let manifest_path = Path::new(&target_package.manifest_path);
    let mut manifest = parse_manifest_only(manifest_path)?;

    let workspace_root_manifest_path = workspace_root.join("Cargo.toml");
    let root_manifest = if manifest.workspace.is_none() && manifest_path != workspace_root_manifest_path {
        parse_manifest_only(&workspace_root_manifest_path).inspect_err(|e| log::error!("{e}")).ok()
    } else { None };

    manifest.complete_from_path_and_workspace(manifest_path, root_manifest.as_ref().map(|ws| (ws, workspace_root.as_path())))
        .map_err(move |e| CargoDebError::TomlParsing(e, manifest_path.to_path_buf()))?;

    Ok(ManifestFound {
        manifest_path: target_package.manifest_path,
        workspace_root_manifest_path,
        build_targets: target_package.targets,
        root_manifest,
        target_dir,
        manifest,
    })
}

/// Returns the workspace metadata based on the `Cargo.toml` that we want to build,
/// and directory that paths may be relative to
fn run_cargo_metadata(manifest_rel_path: Option<&Path>, cargo_locking_flags: CargoLockingFlags) -> CDResult<CargoMetadata> {
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--format-version=1", "--no-deps"]);
    cmd.args(cargo_locking_flags.flags());

    if let Some(path) = manifest_rel_path {
        cmd.args(["--manifest-path".as_ref(), path.as_os_str()]);
    }

    let output = cmd.output()
        .map_err(|e| CargoDebError::CommandFailed(e, "cargo (is it in your PATH?)"))?;
    if !output.status.success() {
        return Err(CargoDebError::CommandError("cargo", "metadata".to_owned(), output.stderr));
    }

    Ok(serde_json::from_slice(&output.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listener::NoOpListener;
    use itertools::Itertools;

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

        let parent = CargoDeb { assets: Some(vec![ original_asset.into() ]), .. Default::default() };
        let variant = CargoDeb { merge_assets: Some(MergeAssets { append: None, by: Some(MergeByKey::Dest(vec![ merge_asset.into() ])) }), .. Default::default() };

        let merged = variant.inherit_from(parent, &NoOpListener);
        let mut merged = merged.assets.expect("should have assets").into_iter().filter_map(|a| a.asset()).collect_vec();
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

        let parent = CargoDeb { assets: Some(vec![ original_asset.into() ]), .. Default::default() };
        let variant = CargoDeb { merge_assets: Some(MergeAssets { append: None, by: Some(MergeByKey::Src(vec![ merge_asset.into() ])) }), .. Default::default() };

        let merged = variant.inherit_from(parent, &NoOpListener);
        let mut merged = merged.assets.expect("should have assets").into_iter().filter_map(|a| a.asset()).collect_vec();
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
        
        let parent = CargoDeb { assets: Some(vec![ original_asset.into() ]), .. Default::default() };
        let variant = CargoDeb { merge_assets: Some(MergeAssets { append: Some(vec![merge_asset.into()]), by: None }), .. Default::default() };
        
        let merged = variant.inherit_from(parent, &NoOpListener);
        let mut merged = merged.assets.expect("should have assets").into_iter().filter_map(|a| a.asset()).collect_vec();

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

        let parent = CargoDeb { assets: Some(vec![ original_asset.into() ]), .. Default::default() };
        let variant = CargoDeb { merge_assets: Some(MergeAssets { append: None, by: Some(MergeByKey::Dest(vec![ merge_asset.clone().into() ])) }), assets: Some(vec![ merge_asset.into(), additional_asset.into() ]), .. Default::default() };

        let merged = variant.inherit_from(parent, &NoOpListener);
        let mut merged = merged.assets.expect("should have assets");
        let merged_asset = merged.remove(0).asset().unwrap();
        assert_eq!("lib/test_variant/empty.txt", merged_asset.source_path.as_os_str(), "should have merged the source location");
        assert_eq!("/opt/test/empty.txt", merged_asset.target_path.as_os_str(), "should preserve dest location");
        assert_eq!(0o655, merged_asset.chmod, "should have merged the dest location");

        let additional_asset = merged.remove(0).asset().unwrap();
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
