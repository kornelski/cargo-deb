use std::env;
use std::env::consts::DLL_PREFIX;
use std::env::consts::DLL_SUFFIX;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use std::io::{BufReader, BufRead, Read, Seek};

/// file extension of the compression format cargo-deb uses unless explicitly specified.
const DEFAULT_COMPRESSION_EXT: &str = "xz";

#[test]
fn build_workspaces() {
    let (cdir, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws1/Cargo.toml", DEFAULT_COMPRESSION_EXT, &["--no-strip", "--fast"]);
    assert!(ddir.path().join("usr/local/bin/renamed2").exists());
    assert!(ddir.path().join("usr/local/bin/decoy").exists());

    let control = fs::read_to_string(cdir.path().join("control")).unwrap();
    assert!(control.contains("Version: 1.0.0-ws-1\n"));
    assert!(control.contains("Package: test1-crate-name\n"));
    assert!(control.contains("Maintainer: ws\n"));

    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws2/Cargo.toml", DEFAULT_COMPRESSION_EXT, &["--no-strip"]);
    assert!(ddir.path().join("usr/bin/renamed2").exists());
    assert!(ddir.path().join(format!("usr/lib/{DLL_PREFIX}test2lib{DLL_SUFFIX}")).exists());
    assert!(ddir.path().join("usr/share/doc/test2/a-read-me").exists());
}

#[test]
fn build_with_explicit_compress_type() {
    // ws1 with gzip
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws1/Cargo.toml", "gz", &["--no-strip", "--compress-type", "gzip"]);
    assert!(ddir.path().join("usr/local/bin/decoy").exists());

    // ws2 with xz
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws2/Cargo.toml", "xz", &["--no-strip", "--compress-type", "xz"]);
    assert!(ddir.path().join("usr/bin/renamed2").exists());
}

#[test]
fn build_with_command_line_compress() {
    // ws1 with system xz
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws1/Cargo.toml", "xz", &["--no-strip", "--compress-system", "--compress-type", "xz"]);
    assert!(ddir.path().join("usr/local/bin/decoy").exists());

    // ws2 with system gzip
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws2/Cargo.toml", "gz", &["--no-strip", "--compress-system", "--compress-type", "gz"]);
    assert!(ddir.path().join("usr/bin/renamed2").exists());
}

#[track_caller]
fn extract_built_package_from_manifest(manifest_path: &str, ext: &str, args: &[&str]) -> (TempDir, TempDir) {
    let (_bdir, deb_path) = cargo_deb(manifest_path, args);
    extract_package(&deb_path, ext)
}

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
    const EXPECTED: &'static [Expected] = &[
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
        [] => {}
        b"\n" if allowed_trailing_nl => {}
        _ => panic!("unexpected trailing data"),
    }
}

#[track_caller]
fn extract_package(deb_path: &Path, ext: &str) -> (TempDir, TempDir) {
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

/// Run `cargo-deb` for the manifest with extra args, returns the `TempDir` holding the built package
/// and the path to the built package.
///
/// The `--manifest-path` and `--output` args are automatically set.
#[track_caller]
fn cargo_deb(manifest_path: &str, args: &[&str]) -> (TempDir, PathBuf) {
    let cargo_dir = tempfile::tempdir().unwrap();
    let deb_path = cargo_dir.path().join("test.deb");

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let cmd_path = root.join(env!("CARGO_BIN_EXE_cargo-deb"));
    assert!(cmd_path.exists());
    let output = Command::new(cmd_path)
        .env("CARGO_TARGET_DIR", cargo_dir.path()) // use isolated 'target' directories
        .arg(format!("--manifest-path={}", root.join(manifest_path).display()))
        .arg(format!("--output={}", deb_path.display()))
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(),
            "Cmd failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

    // prints deb path on the last line
    let last_line = output.stdout[..output.stdout.len() - 1].split(|&c| c == b'\n').last().unwrap();
    let printed_deb_path = Path::new(::std::str::from_utf8(last_line).unwrap());
    assert_eq!(printed_deb_path, deb_path);
    assert!(deb_path.exists());

    (cargo_dir, deb_path)
}

#[test]
#[cfg(all(feature = "lzma", target_family = "unix", not(target_os = "macos")))]
fn run_cargo_deb_command_on_example_dir() {
    let (cdir, ddir) = extract_built_package_from_manifest("example/Cargo.toml", DEFAULT_COMPRESSION_EXT, &[]);

    let control = fs::read_to_string(cdir.path().join("control")).unwrap();
    assert!(control.contains("Package: example\n"));
    assert!(control.contains("Version: 0.1.0-1\n"));
    assert!(control.contains("Section: utils\n"));
    assert!(control.contains("Architecture: "));
    assert!(control.contains("Maintainer: cargo-deb developers <cargo-deb@example.invalid>\n"));

    let md5sums = fs::read_to_string(cdir.path().join("md5sums")).unwrap();
    assert!(md5sums.contains(" usr/bin/example\n"));
    assert!(md5sums.contains(" usr/share/doc/example/changelog.Debian.gz\n"));
    assert!(md5sums.contains("b1946ac92492d2347c6235b4d2611184  var/lib/example/1.txt\n"));
    assert!(md5sums.contains("591785b794601e212b260e25925636fd  var/lib/example/2.txt\n"));
    assert!(md5sums.contains("1537684900f6b12358c88a612adf1049  var/lib/example/3.txt\n"));
    assert!(md5sums.contains("4009d4b433651f005773f43b703b62b1  usr/share/doc/example/copyright\n"), "{md5sums:?}");

    assert!(ddir.path().join("var/lib/example/1.txt").exists());
    assert!(ddir.path().join("var/lib/example/2.txt").exists());
    assert!(ddir.path().join("var/lib/example/3.txt").exists());
    assert!(ddir.path().join("usr/share/doc/example/copyright").exists());
    assert!(ddir.path().join("usr/share/doc/example/changelog.Debian.gz").exists());
    assert!(ddir.path().join("usr/bin/example").exists());
    // changelog.Debian.gz starts with the gzip magic
    assert_eq!(
        &[0x1F, 0x8B],
        &fs::read(ddir.path().join("usr/share/doc/example/changelog.Debian.gz")).unwrap()[..2]
    );
}

#[test]
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
fn run_cargo_deb_command_on_example_dir_with_separate_debug_symbols() {
    let (_cdir, ddir) = extract_built_package_from_manifest("example/Cargo.toml", DEFAULT_COMPRESSION_EXT, &["--separate-debug-symbols"]);

    let stripped = ddir.path().join("usr/bin/example");
    let debug = ddir.path().join("usr/lib/debug/usr/bin/example.debug");

    assert!(stripped.exists());
    assert!(
        debug.exists(),
        "unable to find executable with debug symbols {} for stripped executable {}",
        debug.display(),
        stripped.display()
    );

    let stripped_len = stripped.metadata().unwrap().len();
    let debug_len = debug.metadata().unwrap().len();
    assert!(
        stripped_len < debug_len,
        "stripped executable {stripped_len} should be smaller than debug executable {debug_len}"
    );
}

#[test]
#[cfg(feature = "lzma")]
fn run_cargo_deb_command_on_example_dir_with_variant() {
    let args = ["--variant=debug", "--no-strip"];
    let (_bdir, deb_path) = cargo_deb("example/Cargo.toml", &args);

    let ardir = tempfile::tempdir().unwrap();
    assert!(ardir.path().exists());
    assert!(Command::new("ar")
        .current_dir(ardir.path())
        .arg("-x")
        .arg(deb_path)
        .status().unwrap().success());

    assert_eq!("2.0\n", fs::read_to_string(ardir.path().join("debian-binary")).unwrap());
    let ext = DEFAULT_COMPRESSION_EXT;
    assert!(ardir.path().join(format!("data.tar.{ext}")).exists());
    assert!(ardir.path().join(format!("control.tar.{ext}")).exists());

    let cdir = tempfile::tempdir().unwrap();
    assert!(Command::new("tar")
        .arg("xJf")
        .current_dir(cdir.path())
        .arg(ardir.path().join(format!("control.tar.{ext}")))
        .status().unwrap().success());

    let control = fs::read_to_string(cdir.path().join("control")).unwrap();
    assert!(control.contains("Package: example-debug\n"), "Control is: {control:?}");
    assert!(control.contains("Version: 0.1.0-1\n"));
    assert!(control.contains("Section: utils\n"));
    assert!(control.contains("Architecture: "));
    assert!(control.contains("Maintainer: cargo-deb developers <cargo-deb@example.invalid>\n"));

    let md5sums = fs::read_to_string(cdir.path().join("md5sums")).unwrap();
    assert!(md5sums.contains(" usr/bin/example\n"));
    assert!(md5sums.contains(" usr/share/doc/example-debug/changelog.Debian.gz\n"));
    assert!(md5sums.contains("b1946ac92492d2347c6235b4d2611184  var/lib/example/1.txt\n"));
    assert!(md5sums.contains("591785b794601e212b260e25925636fd  var/lib/example/2.txt\n"));
    assert!(md5sums.contains("835a3c46f2330925774ebf780aa74241  var/lib/example/4.txt\n"));
    assert!(md5sums.contains("72d6fae497d4bb5454bdf0ebe92d45f1  usr/share/doc/example-debug/copyright\n"), "{md5sums:?}");

    let ddir = tempfile::tempdir().unwrap();
    assert!(Command::new("tar")
        .arg("xJf")
        .current_dir(ddir.path())
        .arg(ardir.path().join(format!("data.tar.{ext}")))
        .status().unwrap().success());

    assert!(ddir.path().join("var/lib/example/1.txt").exists());
    assert!(ddir.path().join("var/lib/example/2.txt").exists());
    assert!(ddir.path().join("var/lib/example/4.txt").exists());
    assert!(ddir.path().join("usr/share/doc/example-debug/copyright").exists());
    assert!(ddir.path().join("usr/share/doc/example-debug/changelog.Debian.gz").exists());
    assert!(ddir.path().join("usr/bin/example").exists());
}

#[test]
#[cfg(all(feature = "lzma", target_family = "unix", not(target_os = "macos")))]
fn run_cargo_deb_command_on_example_dir_with_version() {
    let (_bdir, deb_path) = cargo_deb("example/Cargo.toml", &["--deb-version=my-custom-version"]);

    let ardir = tempfile::tempdir().unwrap();
    assert!(ardir.path().exists());
    assert!(Command::new("ar")
        .current_dir(ardir.path())
        .arg("-x")
        .arg(deb_path)
        .status().unwrap().success());

    let ext = DEFAULT_COMPRESSION_EXT;
    assert_eq!("2.0\n", fs::read_to_string(ardir.path().join("debian-binary")).unwrap());
    assert!(ardir.path().join(format!("data.tar.{ext}")).exists());
    assert!(ardir.path().join(format!("control.tar.{ext}")).exists());

    let cdir = tempfile::tempdir().unwrap();
    assert!(Command::new("tar")
        .arg("xf")
        .current_dir(cdir.path())
        .arg(ardir.path().join(format!("control.tar.{ext}")))
        .status().unwrap().success());

    let control = fs::read_to_string(cdir.path().join("control")).unwrap();
    assert!(control.contains("Package: example\n"));
    assert!(control.contains("Version: my-custom-version\n"));
    assert!(control.contains("Section: utils\n"));
    assert!(control.contains("Architecture: "));
    assert!(control.contains("Maintainer: cargo-deb developers <cargo-deb@example.invalid>\n"));

    let md5sums = fs::read_to_string(cdir.path().join("md5sums")).unwrap();
    assert!(md5sums.contains(" usr/bin/example\n"));
    assert!(md5sums.contains(" usr/share/doc/example/changelog.Debian.gz\n"));
    assert!(md5sums.contains("b1946ac92492d2347c6235b4d2611184  var/lib/example/1.txt\n"));
    assert!(md5sums.contains("591785b794601e212b260e25925636fd  var/lib/example/2.txt\n"));
    assert!(md5sums.contains("1537684900f6b12358c88a612adf1049  var/lib/example/3.txt\n"));
    assert!(md5sums.contains("4009d4b433651f005773f43b703b62b1  usr/share/doc/example/copyright\n"), "has:\n{md5sums}");

    let ddir = tempfile::tempdir().unwrap();
    assert!(Command::new("tar")
        .arg("xJf")
        .current_dir(ddir.path())
        .arg(ardir.path().join(format!("data.tar.{ext}")))
        .status().unwrap().success());

    assert!(ddir.path().join("var/lib/example/1.txt").exists());
    assert!(ddir.path().join("var/lib/example/2.txt").exists());
    assert!(ddir.path().join("var/lib/example/3.txt").exists());
    assert!(ddir.path().join("usr/share/doc/example/copyright").exists());
    assert!(ddir.path().join("usr/share/doc/example/changelog.Debian.gz").exists());
    assert!(ddir.path().join("usr/bin/example").exists());
    // changelog.Debian.gz starts with the gzip magic
    assert_eq!(
        &[0x1F, 0x8B],
        &fs::read(ddir.path().join("usr/share/doc/example/changelog.Debian.gz")).unwrap()[..2]
    );
}


fn dir_test_run_in_subdir(subdir_path: &str) {
    let cargo_dir = tempfile::tempdir().unwrap();

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let cmd_path = root.join(env!("CARGO_BIN_EXE_cargo-deb"));
    let deb_path = cargo_dir.path().join("test.deb");

    let output = Command::new(cmd_path)
        .current_dir(root.join(subdir_path))
        .env("CARGO_TARGET_DIR", cargo_dir.path()) // use isolated 'target' directories
        .arg("-p").arg("sub-crate")
        .arg("--no-strip")
        .arg("-q")
        .arg(format!("--output={}", deb_path.display()))
        .output()
        .unwrap();
    assert!(output.status.success(),
        "Cmd failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let (_, ddir) = extract_package(&deb_path, DEFAULT_COMPRESSION_EXT);
    assert!(ddir.path().join("usr/share/doc/sub-crate/README.md").exists(), "must package README");
}

#[test]
fn cwd_dir1() {
    dir_test_run_in_subdir("tests/dir-confusion");
}

#[test]
fn cwd_dir2() {
    dir_test_run_in_subdir("tests/dir-confusion/sub-crate");
}

#[test]
fn cwd_dir3() {
    dir_test_run_in_subdir("tests/dir-confusion/src");
}
