/// This module is a partial implementation of the Debian `DebHelper` command
/// for properly installing systemd sysusers files as part of a .deb package install aka
/// `dh_installsysusers`.
///
/// Upstream only documents the "debian/$package.sysusers" spelling
/// but also accepts "debian/sysusers".
///
/// # See also
///
/// <https://manpages.debian.org/trixie/debhelper/dh_installsysusers.1.en.html>
/// <https://sources.debian.org/src/debhelper/13.24.2/dh_installsysusers>
use std::path::{Path, PathBuf};
use crate::dh::dh_installsystemd::InstallRecipe;
use std::str;

use crate::assets::Asset;
use crate::dh::dh_lib::{autoscript, pkgfile, ScriptFragments};
use crate::listener::Listener;
use crate::util::fname_from_path;
use crate::{CDResult, CargoDebError};

const SYSUSERS_D_DIR: &str = "usr/lib/sysusers.d/";

pub type ConfigFile = Option<(PathBuf, InstallRecipe)>;

/// Find installable systemd sysusers file for the specified debian package
/// in the given directory and return an install
/// recipe for each file detailing the path at which the file should be
/// installed and the mode (chmod) that the file should be given.
pub fn find_config(dir: &Path, main_package: &str) -> ConfigFile {
    let src_path = pkgfile(dir, main_package, main_package, "sysusers", None)?;

    Some((src_path, InstallRecipe {
        path: Path::new(SYSUSERS_D_DIR).join(format!("{main_package}.conf")),
        mode: 0o644,
    }))
}

pub fn generate(package: &str, assets: &[Asset],   scripts: &mut ScriptFragments, listener: &dyn Listener) -> CDResult<()> {
    let mut sysusers_files  = assets
        .iter()
        .filter(|a| a.c.target_path.starts_with(SYSUSERS_D_DIR))
        .map(|v| {
            v.source.source_path()
                .and_then(|p| fname_from_path(&p.with_extension("conf")))
                .ok_or(CargoDebError::Str("dh_installsysusers: invalid source path"))
        })
        .collect::<CDResult<Vec<String>>>()?;

    if sysusers_files.is_empty() {
        return Ok(());
    }

    sysusers_files.sort();

    autoscript(scripts, package, "postinst", "postinst-sysusers",
        &map!{ "CONFILE_BASENAME" => sysusers_files.join(" ") }, false, listener)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{Asset, AssetKind, AssetSource, IsBuilt};
    use crate::util::tests::{add_test_fs_paths, get_read_count, set_test_fs_path_content};

    #[test]
    fn find_units_in_empty_dir_finds_nothing() {
        let pkg_config_file = find_config(Path::new(""), "mypkg");
        assert_eq!(None, pkg_config_file);
    }

    fn assert_eq_found_config(pkg_config_file: &ConfigFile, expected_install_path: &str, source_path: &str) {
        let expected = InstallRecipe {
            path: PathBuf::from(expected_install_path),
            mode: 0o644,
        };
        assert_eq!(*pkg_config_file, Some((PathBuf::from(source_path), expected)));
    }

    #[test]
    fn find_config_for_package() {
        // one of each valid pattern (without a specific unit) and one
        // additional valid pattern with a unit (which should not be matched
        // as we don't specify a specific unit name to match)
        let _g = add_test_fs_paths(&[
            "debian/mypkg.sysusers",
        ]);
        let pkg_config_file = find_config(Path::new("debian"), "mypkg");
        assert_eq_found_config(&pkg_config_file, "usr/lib/sysusers.d/mypkg.conf", "debian/mypkg.sysusers");
    }

    #[test]
    fn generate_with_empty_inputs_does_nothing() {
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_info().times(0).return_const(());

        let mut fragments = ScriptFragments::new();
        generate("", &[],  &mut fragments, &mock_listener).unwrap();

        assert!(fragments.is_empty());
    }

    #[test]
    fn generate_with_arbitrary_asset_does_nothing() {
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_info().times(0).return_const(());

        let assets = vec![Asset::new(
            AssetSource::Path(PathBuf::new()),
            PathBuf::new(),
            Some(0o0),
            IsBuilt::No,
            AssetKind::Any,
        )];

        let mut fragments = ScriptFragments::new();
        generate("mypkg", &assets,  &mut fragments, &mock_listener).unwrap();
        assert!(fragments.is_empty());
    }

    #[test]
    fn generate_with_invalid_tmp_file_asset_fails() {
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_info().times(0).return_const(());

        let assets = vec![Asset::new(
            AssetSource::Path(PathBuf::new()), // path source with empty source path makes no sense
            Path::new("usr/lib/sysusers.d/blah").to_path_buf(),
            Some(0o0),
            IsBuilt::No,
            AssetKind::Any,
        )];

        assert!(generate("mypkg", &assets,  &mut ScriptFragments::new(), &mock_listener).is_err());
    }

    #[test]
    fn generate_with_data_tmp_file_asset_fails() {
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_info().times(0).return_const(());

        let assets = vec![Asset::new(
            AssetSource::Data(vec![]), // only assets of type Path are currently supported
            Path::new("usr/lib/sysusers.d/blah").to_path_buf(),
            Some(0o0),
            IsBuilt::No,
            AssetKind::Any,
        )];

        assert!(generate("mypkg", &assets,  &mut ScriptFragments::new(), &mock_listener).is_err());
    }

    #[test]
    fn generate_with_empty_sysusers_asset() {
        use crate::dh::dh_lib::get_embedded_autoscript;

        const TMP_FILE_NAME: &str = "mypkg.sysusers";
        let tmp_file_path = PathBuf::from(format!("debian/{TMP_FILE_NAME}"));

        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_progress().times(1).return_const(());

        let assets = vec![Asset::new(
            AssetSource::Path(tmp_file_path),
            Path::new("usr/lib/sysusers.d/blah").to_path_buf(),
            Some(0o0),
            IsBuilt::No,
            AssetKind::Any,
        )];

        let mut fragments = ScriptFragments::new();
        generate("mypkg", &assets, &mut fragments, &mock_listener).unwrap();
        assert_eq!(1, fragments.len());

        let (fragment_name, created_text) = fragments.into_iter().next().unwrap();

        // should create an augmentation for the postinst script
        assert_eq!("mypkg.postinst.debhelper", fragment_name);

        // Verify the created script contents. It should have two lines
        // more than the autoscript fragment it was based on, like so:
        //   # Automatically added by ...
        //   <autoscript fragment lines with placeholders replaced>
        //   # End automatically added section
        let autoscript_text = get_embedded_autoscript("postinst-sysusers");
        let autoscript_line_count = autoscript_text.lines().count();
        let created_line_count = created_text.lines().count();
        assert_eq!(autoscript_line_count + 2, created_line_count);

        // Verify the content of the added comment lines
        let mut lines = created_text.lines();
        assert!(lines.next().unwrap().starts_with("# Automatically added by"));
        assert_eq!(lines.nth_back(0).unwrap(), "# End automatically added section");

        // Check that the autoscript fragment lines were properly copied
        // into the created script complete with expected substitutions
        let expected_autoscript_text = autoscript_text.replace("#CONFILE_BASENAME#", TMP_FILE_NAME.replace(".sysusers", ".conf").as_str());
        let expected_autoscript_text = expected_autoscript_text.trim_end();
        let start1 = 1;
        let end1 = start1 + autoscript_line_count;
        let created_autoscript_text = created_text.lines().collect::<Vec<&str>>()[start1..end1].join("\n");
        assert_ne!(expected_autoscript_text, autoscript_text);
        assert_eq!(expected_autoscript_text, created_autoscript_text);
    }

    #[test]
    fn generate_acts_only_on_config_files_with_the_expected_install_path() {
        // Note: find_units() will set the target path correctly.
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_info().times(0).return_const(());

        let assets = vec![Asset::new(
            AssetSource::Path(PathBuf::from("debian/mypkg.sysusers")),
            Path::new("some/other/path/").to_path_buf(),
            Some(0o0),
            IsBuilt::No,
            AssetKind::Any,
        )];

        let mut fragments = ScriptFragments::new();
        generate("mypkg", &assets, &mut fragments, &mock_listener).unwrap();
        assert_eq!(0, fragments.len());
    }

    #[test]
    fn generate_creates_expected_autoscript_fragments() {
        let config_file_path = "debian/mypkg.sysusers";

        // setup input for generate()
        let assets = vec![Asset::new(
            AssetSource::Path(PathBuf::from(config_file_path)),
            format!("usr/lib/sysusers.d/mypkg.conf").into(),
            Some(0o0),
            IsBuilt::No,
            AssetKind::Any,
        )];

        // setup mocks
        let mut mock_listener = crate::listener::MockListener::new();
        mock_listener.expect_progress().return_const(());

        let config_file_content = "u ego -\n".to_owned();
        set_test_fs_path_content(config_file_path, config_file_content);

        // Add all Autoscript paths to the in-memory test file system so that
        // we can track whether they are read or not.
        let _g = add_test_fs_paths(&[
            "postinst-sysusers",
        ]);

        // generate!
        let mut fragments = ScriptFragments::new();
        generate("mypkg", &assets, &mut fragments, &mock_listener).unwrap();

        // verify, though don't verify creation of autoscript fragments as that
        // is verified in tests of the lower level functionality, instead verify
        // only that the generate() logic creates the expected named fragments
        // and while doing so read the expected autoscript files the expected
        // number of times.

        // Perl dh_installsysusers installs the postinst-sysusers fragment
        // as long as there's at least 1 sysusers config file.

        let mut autoscript_fragments_to_check_for = std::collections::HashSet::new();

                assert_eq!(1, get_read_count("postinst-sysusers"));
                autoscript_fragments_to_check_for.insert("postinst.debhelper");

        for autoscript in &autoscript_fragments_to_check_for {
            let key = format!("mypkg.{autoscript}");
            assert!(fragments.contains_key(&key), "{}", key);
        }
    }
}
