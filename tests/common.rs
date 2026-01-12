use std::{env, fs, io::{BufRead as _, BufReader, Read as _, Seek as _}, path::{Path, PathBuf}, process::Command};

use tempfile::TempDir;

/// file extension of the compression format cargo-deb uses unless explicitly specified.
pub const DEFAULT_COMPRESSION_EXT: &str = "xz";

#[track_caller]
fn check_ar(deb_path: &Path) {
    let mut file = BufReader::new(fs::File::open(deb_path).unwrap());
    let mut line = String::new();
    file.read_line(&mut line).unwrap();
    assert_eq!(line, "!<arch>\n");
    struct Expected {
        name_prefix: &'static str,
        data: Option<&'static [u8]>,
    }
    const EXPECTED: &[Expected] = &[
        Expected {
            name_prefix: "debian-binary   ",
            data: Some(b"2.0\n"),
        },
        Expected {
            name_prefix: "control.tar.",
            data: None,
        },
        Expected {
            name_prefix: "data.tar.",
            data: None,
        },
    ];
    let mut data = Vec::new();
    for expected in EXPECTED {
        if file.stream_position().unwrap() % 2 != 0 {
            line.clear();
            file.read_line(&mut line).unwrap();
            assert_eq!(line, "\n");
        }
        line.clear();
        file.read_line(&mut line).unwrap();
        assert_eq!(line.len(), 60);
        let name = &line[..16];
        assert!(name.starts_with(expected.name_prefix));
        let mtime_str = &line[16..28];
        let mtime: u64 = mtime_str.trim().parse().unwrap();
        assert_eq!(mtime_str, format!("{mtime:<12}"));
        let owner_id = &line[28..34];
        assert_eq!(owner_id, "0     ");
        let group_id = &line[34..40];
        assert_eq!(group_id, "0     ");
        let file_type_and_mode = &line[40..48];
        assert_eq!(file_type_and_mode, "100644  "); // dpkg uses 100644
        let file_size_str = &line[48..58];
        let file_size: u64 = file_size_str.trim().parse().unwrap();
        assert_eq!(file_size_str, format!("{file_size:<10}"));
        data.resize(file_size.try_into().unwrap(), 0);
        file.read_exact(&mut data).unwrap();
        if let Some(expected_data) = expected.data {
            assert_eq!(data, expected_data);
        }
    }
    let allowed_trailing_nl = file.stream_position().unwrap() % 2 != 0;
    data.clear();
    file.read_to_end(&mut data).unwrap();
    match &*data {
        [] => {},
        b"\n" if allowed_trailing_nl => {},
        _ => panic!("unexpected trailing data"),
    }
}


#[track_caller]
pub fn extract_package(deb_path: &Path, ext: &str) -> (TempDir, TempDir) {
    check_ar(deb_path);
    let ardir = tempfile::tempdir().expect("testdir");
    assert!(ardir.path().exists());
    assert!(Command::new("ar")
        .current_dir(ardir.path())
        .arg("-x")
        .arg(deb_path)
        .status().unwrap().success());

    assert_eq!("2.0\n", fs::read_to_string(ardir.path().join("debian-binary")).unwrap());

    assert!(ardir.path().join(format!("data.tar.{ext}")).exists());
    assert!(ardir.path().join(format!("control.tar.{ext}")).exists());

    let cdir = tempfile::tempdir().unwrap();
    assert!(Command::new("tar")
        .arg("xf")
        .current_dir(cdir.path())
        .arg(ardir.path().join(format!("control.tar.{ext}")))
        .status().unwrap().success());

    let ddir = tempfile::tempdir().unwrap();
    assert!(Command::new("tar")
        .arg("xf")
        .current_dir(ddir.path())
        .arg(ardir.path().join(format!("data.tar.{ext}")))
        .status().unwrap().success());

    (cdir, ddir)
}

pub fn dir_test_run_in_subdir(subdir_path: &str) -> TempDir {
    let _ = env_logger::builder().is_test(true).try_init();

    let cargo_dir = tempfile::tempdir().unwrap();

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let cmd_path = root.join(env!("CARGO_BIN_EXE_cargo-deb"));
    let deb_path = cargo_dir.path().join("test.deb");

    let mut cmd = Command::new(cmd_path);

    let output = cmd
        .current_dir(root.join(subdir_path))
        .env("CARGO_TARGET_DIR", cargo_dir.path()) // use isolated 'target' directories
        .env("CARGO_BUILD_BUILD_DIR", cargo_dir.path().join("build-tmp")) // use isolated build directories
        .arg("-p").arg("sub-crate")
        .arg("--no-strip")
        .arg("-q")
        .arg(format!("--output={}", deb_path.display()))
        .output()
        .unwrap();
    if !output.status.success() {
        panic!(
            "Cmd failed: {} {cmd:?}\n{}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            cargo_dir.keep().display(),
        );
    }

    let (_, ddir) = extract_package(&deb_path, DEFAULT_COMPRESSION_EXT);
    assert!(ddir.path().join("usr/share/doc/sub-crate/README.md").exists(), "must package README");

    ddir
}

