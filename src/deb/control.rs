use crate::config::{Config, PackageConfig};
use crate::deb::tar::Tarball;
use crate::dh::{dh_installsystemd, dh_lib};
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::util::{is_path_file, read_file_to_bytes};
use dh_lib::ScriptFragments;
use std::fs;
use std::io::Write;
use std::path::Path;

pub struct ControlArchiveBuilder<'l, W: Write> {
    archive: Tarball<W>,
    listener: &'l dyn Listener,
}

impl<'l, W: Write> ControlArchiveBuilder<'l, W> {
    pub fn new(dest: W, time: u64, listener: &'l dyn Listener) -> Self {
        Self {
            archive: Tarball::new(dest, time),
            listener,
        }
    }

    /// Generates an uncompressed tar archive with `control`, and others
    pub fn generate_archive(&mut self, config: &Config, package_deb: &PackageConfig) -> CDResult<()> {
        self.add_control(&package_deb.generate_control(config)?)?;

        if let Some(files) = package_deb.conf_files() {
            self.add_conf_files(&files)?;
        }

        self.generate_scripts(config, package_deb)?;
        if let Some(rel_path) = &package_deb.triggers_file_rel_path {
            self.add_triggers_file(config, rel_path)?;
        }
        Ok(())
    }

    pub fn finish(self) -> CDResult<W> {
        Ok(self.archive.into_inner()?)
    }

    /// Append Debian maintainer script files (control, preinst, postinst, prerm,
    /// postrm and templates) present in the `maintainer_scripts` path to the
    /// archive, if `maintainer_scripts` is configured.
    ///
    /// Additionally, when `systemd_units` is configured, shell script fragments
    /// "for enabling, disabling, starting, stopping and restarting systemd unit
    /// files" (quoting `man 1 dh_installsystemd`) will replace the `#DEBHELPER#`
    /// token in the provided maintainer scripts.
    ///
    /// If a shell fragment cannot be inserted because the target script is missing
    /// then the entire script will be generated and appended to the archive.
    ///
    /// # Requirements
    ///
    /// When `systemd_units` is configured, user supplied `maintainer_scripts` must
    /// contain a `#DEBHELPER#` token at the point where shell script fragments
    /// should be inserted.
    fn generate_scripts(&mut self, config: &Config, package_deb: &PackageConfig) -> CDResult<()> {
        if let Some(ref maintainer_scripts_dir) = package_deb.maintainer_scripts_rel_path {
            let maintainer_scripts_dir = config.path_in_package(maintainer_scripts_dir);
            let mut scripts = ScriptFragments::with_capacity(0);

            if let Some(systemd_units_config_vec) = &package_deb.systemd_units {
                for systemd_units_config in systemd_units_config_vec {
                    // Select and populate autoscript templates relevant to the unit
                    // file(s) in this package and the configuration settings chosen.
                    scripts = dh_installsystemd::generate(
                        &package_deb.deb_name,
                        &package_deb.assets.resolved,
                        &dh_installsystemd::Options::from(systemd_units_config),
                        self.listener,
                    )?;

                    // Get Option<&str> from Option<String>
                    let unit_name = systemd_units_config.unit_name.as_deref();

                    // Replace the #DEBHELPER# token in the users maintainer scripts
                    // and/or generate maintainer scripts from scratch as needed.
                    dh_lib::apply(
                        &maintainer_scripts_dir,
                        &mut scripts,
                        &package_deb.deb_name,
                        unit_name,
                        self.listener,
                    )?;
                }
            }

            // Add maintainer scripts to the archive, either those supplied by the
            // user or if available prefer modified versions generated above.
            for name in ["config", "preinst", "postinst", "prerm", "postrm", "templates"] {
                let script_path;
                let (contents, source_path) = if let Some(script) = scripts.remove(name) {
                    (script, Some("systemd_units"))
                } else {
                    script_path = maintainer_scripts_dir.join(name);
                    if !is_path_file(&script_path) {
                        continue;
                    }
                    (read_file_to_bytes(&script_path)?, script_path.to_str())
                };

                // The config, postinst, postrm, preinst, and prerm
                // control files should use mode 0755; all other control files should use 0644.
                // See Debian Policy Manual section 10.9
                // and lintian tag control-file-has-bad-permissions
                let permissions = if name == "templates" { 0o644 } else { 0o755 };
                self.add_file_with_log(name.as_ref(), &contents, permissions, source_path)?;
            }
        }

        Ok(())
    }

    fn add_file_with_log(&mut self, name: &Path, contents: &[u8], permissions: u32, source_path: Option<&str>) -> CDResult<()> {
        self.listener.info(format!("{} -> {}", source_path.unwrap_or("-"), name.display()));
        self.archive.file(name, contents, permissions)
    }

    // Add the control file to the tar archive.
    fn add_control(&mut self, control: &[u8]) -> CDResult<()> {
        self.archive.file("./control", control, 0o644)?;
        Ok(())
    }

    /// If configuration files are required, the conffiles file will be created.
    fn add_conf_files(&mut self, list: &str) -> CDResult<()> {
        self.add_file_with_log("./conffiles".as_ref(), list.as_bytes(), 0o644, None)
    }

    fn add_triggers_file(&mut self, config: &Config, rel_path: &Path) -> CDResult<()> {
        let path = config.path_in_package(rel_path);
        let content = match fs::read(&path) {
            Ok(p) => p,
            Err(e) => return Err(CargoDebError::IoFile("triggers file", e, path)),
        };
        self.add_file_with_log("./triggers".as_ref(), &content, 0o644, path.to_str())
    }
}

#[cfg(test)]
mod tests {
    // The following test suite verifies that `fn generate_scripts()` correctly
    // copies "maintainer scripts" (files with the name config, preinst, postinst,
    // prerm, postrm, and/or templates) from the `maintainer_scripts` directory
    // into the generated archive, and in the case that a systemd config is
    // provided, that a service file when present causes #DEBHELPER# placeholders
    // in the maintainer scripts to be replaced and missing maintainer scripts to
    // be generated.
    //
    // The exact details of maintainer script replacement is tested
    // in `dh_installsystemd.rs`, here we are more interested in testing that
    // `fn generate_scripts()` correctly looks for maintainer script and unit
    // script files relative to the crate root, whether processing the root crate
    // or a workspace member crate.
    //
    // This test depends on the existence of two test crates organized such that
    // one is a Cargo workspace member and the other is a root crate.
    //
    //   test-resources/
    //     testroot/         <-- root crate
    //       Cargo.toml
    //       testchild/      <-- workspace member crate
    //         Cargo.toml

    use super::*;
    use crate::assets::{Asset, AssetSource, IsBuilt};
    use crate::listener::MockListener;
    use crate::parse::manifest::SystemdUnitsConfig;
    use crate::util::tests::{add_test_fs_paths, set_test_fs_path_content};
    use crate::CargoLockingFlags;
    use std::collections::HashMap;
    use std::io::prelude::Read;
    use std::path::PathBuf;

    fn filename_from_path_str(path: &str) -> String {
        Path::new(path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    fn decode_name<R>(entry: &tar::Entry<'_, R>) -> String where R: Read {
        std::str::from_utf8(&entry.path_bytes()).unwrap().to_string()
    }

    fn decode_names<R>(ar: &mut tar::Archive<R>) -> Vec<String> where R: Read {
        ar.entries().unwrap().map(|e| decode_name(&e.unwrap())).collect()
    }

    fn extract_contents<R>(ar: &mut tar::Archive<R>) -> HashMap<String, String> where R: Read {
        let mut out = HashMap::new();
        for entry in ar.entries().unwrap() {
            let mut unwrapped = entry.unwrap();
            let name = decode_name(&unwrapped);
            let mut buf = Vec::new();
            unwrapped.read_to_end(&mut buf).unwrap();
            let content = String::from_utf8(buf).unwrap();
            out.insert(name, content);
        }
        out
    }

    #[track_caller]
    #[cfg(test)]
    fn prepare<'l, W: Write>(dest: W, package_name: Option<&str>, mock_listener: &'l mut MockListener) -> (Config, PackageConfig, ControlArchiveBuilder<'l, W>) {
        mock_listener.expect_info().return_const(());

        let (mut config, mut package_deb) = Config::from_manifest(
            Some(Path::new("test-resources/testroot/Cargo.toml")),
            package_name,
            None,
            None,
            None,
            Default::default(),
            None,
            None,
            None,
            CargoLockingFlags::default(),
            mock_listener,
        )
        .unwrap();
        config.prepare_assets_before_build(&mut package_deb, mock_listener).unwrap();

        // make the absolute manifest dir relative to our crate root dir
        // as the static paths we receive from the caller cannot be set
        // to the absolute path we find ourselves in at test run time, but
        // instead have to match exactly the paths looked up based on the
        // value of the manifest dir.
        config.package_manifest_dir = config.package_manifest_dir.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap().to_path_buf();

        let ar = ControlArchiveBuilder::new(dest, 0, mock_listener);

        (config, package_deb, ar)
    }

    #[test]
    fn generate_scripts_does_nothing_if_maintainer_scripts_is_not_set() {
        let mut listener = MockListener::new();
        let (config, package_deb, mut in_ar) = prepare(vec![], None, &mut listener);

        // supply a maintainer script as if it were available on disk
        let _g = add_test_fs_paths(&["debian/postinst"]);

        // generate scripts and store them in the given archive
        in_ar.generate_scripts(&config, &package_deb).unwrap();

        // finish the archive and unwrap it as a byte vector
        let archive_bytes = in_ar.finish().unwrap();

        // parse the archive bytes
        let mut out_ar = tar::Archive::new(&archive_bytes[..]);

        // compare the file names in the archive to what we expect
        let archived_file_names = decode_names(&mut out_ar);
        assert!(archived_file_names.is_empty());
    }

    #[test]
    fn generate_scripts_archives_user_supplied_maintainer_scripts_in_root_package() {
        let maintainer_script_paths = vec![
            "test-resources/testroot/debian/config",
            "test-resources/testroot/debian/preinst",
            "test-resources/testroot/debian/postinst",
            "test-resources/testroot/debian/prerm",
            "test-resources/testroot/debian/postrm",
            "test-resources/testroot/debian/templates",
        ];
        generate_scripts_for_package_without_systemd_unit(None, &maintainer_script_paths);
    }

    #[test]
    fn generate_scripts_archives_user_supplied_maintainer_scripts_in_workspace_package() {
        let maintainer_script_paths = vec![
            "test-resources/testroot/testchild/debian/config",
            "test-resources/testroot/testchild/debian/preinst",
            "test-resources/testroot/testchild/debian/postinst",
            "test-resources/testroot/testchild/debian/prerm",
            "test-resources/testroot/testchild/debian/postrm",
            "test-resources/testroot/testchild/debian/templates",
        ];
        generate_scripts_for_package_without_systemd_unit(Some("test_child"), &maintainer_script_paths);
    }

    #[track_caller]
    fn generate_scripts_for_package_without_systemd_unit(package_name: Option<&str>, maintainer_script_paths: &[&'static str]) {
        let mut listener = MockListener::new();
        let (config, mut package_deb, mut in_ar) = prepare(vec![], package_name, &mut listener);

        // supply a maintainer script as if it were available on disk
        // provide file content that we can easily verify
        for script in maintainer_script_paths {
            let content = format!("some contents: {script}");
            set_test_fs_path_content(script, content.clone());
        }

        // specify a path relative to the (root or workspace child) package
        package_deb
            .maintainer_scripts_rel_path
            .get_or_insert(PathBuf::from("debian"));

        // generate scripts and store them in the given archive
        in_ar.generate_scripts(&config, &package_deb).unwrap();

        // finish the archive and unwrap it as a byte vector
        let archive_bytes = in_ar.finish().unwrap();

        // parse the archive bytes
        let mut out_ar = tar::Archive::new(&archive_bytes[..]);

        // compare the file contents in the archive to what we expect
        let archived_content = extract_contents(&mut out_ar);

        assert_eq!(maintainer_script_paths.len(), archived_content.len());

        // verify that the content we supplied was faithfully archived
        for script in maintainer_script_paths {
            let expected_content = &format!("some contents: {script}");
            let filename = filename_from_path_str(script);
            let actual_content = archived_content.get(&filename).unwrap();
            assert_eq!(expected_content, actual_content);
        }
    }

    #[test]
    fn generate_scripts_augments_maintainer_scripts_for_unit_in_root_package() {
        let maintainer_scripts = vec![
            ("test-resources/testroot/debian/config", Some("dummy content")),
            ("test-resources/testroot/debian/preinst", Some("dummy content\n#DEBHELPER#")),
            ("test-resources/testroot/debian/postinst", Some("dummy content\n#DEBHELPER#")),
            ("test-resources/testroot/debian/prerm", Some("dummy content\n#DEBHELPER#")),
            ("test-resources/testroot/debian/postrm", Some("dummy content\n#DEBHELPER#")),
            ("test-resources/testroot/debian/templates", Some("dummy content")),
        ];
        generate_scripts_for_package_with_systemd_unit(None, &maintainer_scripts, "test-resources/testroot/debian/some.service");
    }

    #[test]
    fn generate_scripts_augments_maintainer_scripts_for_unit_in_workspace_package() {
        let maintainer_scripts = vec![
            ("test-resources/testroot/testchild/debian/config", Some("dummy content")),
            ("test-resources/testroot/testchild/debian/preinst", Some("dummy content\n#DEBHELPER#")),
            ("test-resources/testroot/testchild/debian/postinst", Some("dummy content\n#DEBHELPER#")),
            ("test-resources/testroot/testchild/debian/prerm", Some("dummy content\n#DEBHELPER#")),
            ("test-resources/testroot/testchild/debian/postrm", Some("dummy content\n#DEBHELPER#")),
            ("test-resources/testroot/testchild/debian/templates", Some("dummy content")),
        ];
        generate_scripts_for_package_with_systemd_unit(
            Some("test_child"),
            &maintainer_scripts,
            "test-resources/testroot/testchild/debian/some.service",
        );
    }

    #[test]
    fn generate_scripts_generates_missing_maintainer_scripts_for_unit_in_root_package() {
        let maintainer_scripts = vec![
            ("test-resources/testroot/debian/postinst", None),
            ("test-resources/testroot/debian/prerm", None),
            ("test-resources/testroot/debian/postrm", None),
        ];
        generate_scripts_for_package_with_systemd_unit(None, &maintainer_scripts, "test-resources/testroot/debian/some.service");
    }

    #[test]
    fn generate_scripts_generates_missing_maintainer_scripts_for_unit_in_workspace_package() {
        let maintainer_scripts = vec![
            ("test-resources/testroot/testchild/debian/postinst", None),
            ("test-resources/testroot/testchild/debian/prerm", None),
            ("test-resources/testroot/testchild/debian/postrm", None),
        ];
        generate_scripts_for_package_with_systemd_unit(
            Some("test_child"),
            &maintainer_scripts,
            "test-resources/testroot/testchild/debian/some.service",
        );
    }

    // `maintainer_scripts` is a collection of file system paths for which:
    //   - each file should be in the same directory
    //   - the generated archive should contain a file with each of the given filenames
    //   - if Some(...) then pretend when creating the archive that a file at that path exists with the given content
    #[track_caller]
    fn generate_scripts_for_package_with_systemd_unit(
        package_name: Option<&str>,
        maintainer_scripts: &[(&'static str, Option<&'static str>)],
        service_file: &'static str,
    ) {
        let mut listener = MockListener::new();
        let (config, mut package_deb, mut in_ar) = prepare(vec![], package_name, &mut listener);

        // supply a maintainer script as if it were available on disk
        // provide file content that we can easily verify
        for &(script, content) in maintainer_scripts {
            if let Some(content) = content {
                set_test_fs_path_content(script, content.to_string());
            }
        }

        set_test_fs_path_content(service_file, "mock service file".to_string());

        // make the unit file available for systemd unit processing
        let source = AssetSource::Path(PathBuf::from(service_file));
        let target_path = PathBuf::from(format!("lib/systemd/system/{}", filename_from_path_str(service_file)));
        package_deb.assets.resolved.push(Asset::new(source, target_path, 0o000, IsBuilt::No, false));

        // look in the current dir for maintainer scripts (none, but the systemd
        // unit processing will be skipped if we don't set this)
        package_deb.maintainer_scripts_rel_path.get_or_insert(PathBuf::from("debian"));

        // enable systemd unit processing
        package_deb.systemd_units.get_or_insert(vec![SystemdUnitsConfig::default()]);

        // generate scripts and store them in the given archive
        in_ar.generate_scripts(&config, &package_deb).unwrap();

        // finish the archive and unwrap it as a byte vector
        let archive_bytes = in_ar.finish().unwrap();

        // check that the expected files were included in the archive
        let mut out_ar = tar::Archive::new(&archive_bytes[..]);

        let mut archived_file_names = decode_names(&mut out_ar);
        archived_file_names.sort();

        let mut expected_maintainer_scripts = maintainer_scripts
            .iter()
            .map(|(script, _)| filename_from_path_str(script))
            .collect::<Vec<String>>();
        expected_maintainer_scripts.sort();

        assert_eq!(expected_maintainer_scripts, archived_file_names);

        // check the content of the archived files for any unreplaced placeholders.
        // create a new tar wrapper around the bytes as you cannot seek the same
        // Archive more than once.
        let mut out_ar = tar::Archive::new(&archive_bytes[..]);

        let unreplaced_placeholders = out_ar
            .entries()
            .unwrap()
            .map(Result::unwrap)
            .map(|mut entry| {
                let mut v = String::new();
                entry.read_to_string(&mut v).unwrap();
                v
            })
            .any(|v| v.contains("#DEBHELPER#"));

        assert!(!unreplaced_placeholders);
    }
}
