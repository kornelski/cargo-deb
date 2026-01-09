use std::path::{Path, PathBuf};

use cargo_deb::assets::{RawAsset, RawAssetOrAuto};
use cargo_deb::parse::manifest::CargoDeb as ManifestCargoDeb;

const PACKAGE_DEB_GEN: &str = "package-deb-gen";
const ASSETS_DIR: &str = "assets";
const ASSETS_PKG_ROOT_DIR: &str = "/etc/package/id/2137/";

fn non_empty_assets(path: &Path) -> Vec<RawAssetOrAuto> {
    let dir_contents = path.read_dir().unwrap();

    let mut result = vec![];

    for elem in dir_contents.into_iter().filter(Result::is_ok).map(Result::unwrap) {
        let contents = std::fs::read(elem.path());

        // not using let chains, because of potential MSRV issues
        if let Ok(contents) = contents {
            let string = String::try_from(contents);
            if string.is_ok_and(|string| !string.trim().is_empty()) {
                result.push(RawAssetOrAuto::RawAsset(RawAsset {
                    source_path: elem.path(),
                    target_path: PathBuf::from(ASSETS_PKG_ROOT_DIR).join(elem.file_name().into_string().unwrap()),
                    chmod: 666,
                }));
            }
        }
    }

    result
}

#[test]
pub fn check_serialization_deserialization() {
    let current_dir = PathBuf::from(file!()).parent().unwrap().to_owned();
    let assets_dir = current_dir.join(PACKAGE_DEB_GEN).join(ASSETS_DIR);

    // example of dynamically chosen assets
    let assets = Some(non_empty_assets(&assets_dir));

    let manifest = ManifestCargoDeb {
        name: Some("my-package-name".into()),
        maintainer: Some("anon".into()),
        assets,
        ..Default::default()
    }.try_into_cargo_toml("0.1.0", Some("a")).unwrap();

    println!("{}", toml::to_string_pretty(&manifest).unwrap());
    assert!(false);
}
