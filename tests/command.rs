use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use std::io::{BufRead, BufReader, Read, Seek};
use tempfile::TempDir;

/// file extension of the compression format cargo-deb uses unless explicitly specified.
const DEFAULT_COMPRESSION_EXT: &str = "xz";
const DEFAULT_STRIP: &str = if cfg!(all(target_family = "unix", not(target_os = "macos"))) { "--strip" } else { "--no-strip" };

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

    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws2/Cargo.toml", DEFAULT_COMPRESSION_EXT, &["--no-strip", "--multiarch=same"]);
    assert!(ddir.path().join("usr/bin/renamed2").exists());
    assert!(ddir.path().join(format!("usr/lib/{}-linux-gnu/{DLL_PREFIX}test2lib{DLL_SUFFIX}", std::env::consts::ARCH)).exists());
    assert!(ddir.path().join("usr/share/doc/test2/a-read-me").exists());
}

#[test]
fn default_selection() {
    let (cdir, ddir) = extract_built_package_from_manifest("tests/ws-metadata/Cargo.toml", DEFAULT_COMPRESSION_EXT, &["--maintainer=x", "--no-strip"]);
    assert!(ddir.path().join("usr/bin/c1").exists());
    assert!(!ddir.path().join(format!("usr/lib/{DLL_PREFIX}c2{DLL_SUFFIX}")).exists());

    let control = fs::read_to_string(cdir.path().join("control")).unwrap();
    assert!(control.contains("Package: hello\n"), "{control}");
    assert!(control.contains("Maintainer: x\n"));
}

#[test]
fn build_with_explicit_compress_type_gz() {
    let _ = env_logger::builder().is_test(true).try_init();

    // ws1 with gzip
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws1/Cargo.toml", "gz", &["--no-strip", "--compress-type", "gzip"]);
    assert!(ddir.path().join("usr/local/bin/decoy").exists());
}

#[test]
fn build_with_explicit_compress_type_xz() {
    // ws2 with xz
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws2/Cargo.toml", "xz", &["--no-strip", "--compress-type", "xz"]);
    assert!(ddir.path().join("usr/bin/renamed2").exists());
}

#[test]
fn build_with_command_line_compress_xz() {
    // ws1 with system xz
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws1/Cargo.toml", "xz", &["--no-strip", "--compress-system", "--compress-type", "xz"]);
    assert!(ddir.path().join("usr/local/bin/decoy").exists());
}

#[test]
fn build_with_command_line_compress_gz() {
    // ws2 with system gzip
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws2/Cargo.toml", "gz", &["--no-strip", "--compress-system", "--compress-type", "gz"]);
    assert!(ddir.path().join("usr/bin/renamed2").exists());
}

#[test]
#[cfg_attr(any(not(target_family = "unix"), target_os = "macos"), ignore = "needs linux strip command")]
fn no_dbgsym() {
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws2/Cargo.toml", "xz", &["--no-dbgsym", "--fast"]);
    assert!(ddir.path().join("usr/bin/renamed2").exists());
}

#[test]
fn no_dbgsym_strip() {
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws2/Cargo.toml", "xz", &["--no-dbgsym", "--fast", DEFAULT_STRIP]);
    assert!(ddir.path().join("usr/bin/renamed2").exists());
}

#[test]
#[cfg_attr(any(not(target_family = "unix"), target_os = "macos"), ignore = "needs linux strip command")]
fn no_symbols_for_dbgsym() {
    let (_, ddir) = extract_built_package_from_manifest("tests/test-workspace/test-ws2/Cargo.toml", "xz", &["--dbgsym", "--override-debug=none", "--fast"]);
    assert!(ddir.path().join("usr/bin/renamed2").exists());
}

#[track_caller]
fn extract_built_package_from_manifest(manifest_path: &str, ext: &str, args: &[&str]) -> (TempDir, TempDir) {
    let (tmpdir, deb_path, ddeb) = cargo_deb(manifest_path, args);
    if let Some(ddeb) = ddeb {
        drop(tmpdir.keep());
        panic!("{ddeb:?} built unexpectedly");
    }
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
/// and the path to the built package, optional ddeb package.
///
/// The `--manifest-path` and `--output` args are automatically set.
#[track_caller]
fn cargo_deb(manifest_path: &str, args: &[&str]) -> (TempDir, PathBuf, Option<PathBuf>) {
    let _ = env_logger::builder().is_test(true).try_init();

    let cargo_dir = tempfile::tempdir().unwrap();
    assert!(cargo_dir.path().is_absolute());
    let deb_path = cargo_dir.path().join("test.deb");

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let cmd_path = root.join(env!("CARGO_BIN_EXE_cargo-deb"));
    assert!(cmd_path.exists());

    let workdir = root.join(Path::new(manifest_path).parent().unwrap());
    let output = Command::new(cmd_path)
        .env("CARGO_TARGET_DIR", cargo_dir.path()) // use isolated 'target' directories
        .arg(format!("--output={}", deb_path.display()))
        .args(args)
        .current_dir(workdir)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{manifest_path} {args:?}: {stdout}");
    eprintln!("{manifest_path} {args:?}: {stderr}");

    assert!(output.status.success(), "Cmd failed {stdout}\n{stderr}");

    // prints deb path on the last line
    let mut lines = stdout.lines();
    let printed_deb_path = Path::new(lines.next_back().unwrap());
    let before_last_line = lines.next_back().unwrap_or_default();
    let maybe_ddeb_path = if before_last_line.ends_with(".ddeb") { Some(PathBuf::from(before_last_line)) } else { None };
    assert_eq!(printed_deb_path, deb_path);
    assert!(deb_path.exists());

    (cargo_dir, deb_path, maybe_ddeb_path)
}

#[test]
#[cfg(all(feature = "lzma", target_family = "unix", not(target_os = "macos")))]
fn run_cargo_deb_command_on_example_dir() {
    let (cdir, ddir) = extract_built_package_from_manifest("example/Cargo.toml", DEFAULT_COMPRESSION_EXT, &["--no-separate-debug-symbols"]);

    let control = fs::read_to_string(cdir.path().join("control")).unwrap();
    assert!(control.contains("Package: example\n"));
    assert!(control.contains("Version: 0.1.0-1\n"));
    assert!(control.contains("Section: utils\n"));
    assert!(control.contains("Architecture: "));
    assert!(control.contains("Maintainer: cargo-deb developers <cargo-deb@example.invalid>\n"));

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
#[cfg_attr(any(not(target_family = "unix"), target_os = "macos"), ignore = "needs linux strip command")]
fn run_cargo_deb_command_on_example_dir_with_separate_debug_symbols() {
    let (_cdir, ddir) = extract_built_package_from_manifest("example/Cargo.toml", DEFAULT_COMPRESSION_EXT, &["--separate-debug-symbols", "--no-dbgsym"]);

    let stripped = ddir.path().join("usr/bin/example");
    assert!(stripped.exists());

    let debug = glob::glob(ddir.path().join("usr/lib/debug/.build-id/*/*.debug").to_str().unwrap())
        .unwrap().flatten().next().expect("can't find gnu-debuglink file in usr/lib/debug/.build-id/");

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
#[cfg_attr(any(not(target_family = "unix"), target_os = "macos"), ignore = "needs linux strip command")]
fn run_cargo_deb_command_on_example_dir_with_dbgsym() {
    let (_bdir, deb_path, ddeb_path) = cargo_deb("example/Cargo.toml", &["--dbgsym", "--deb-revision=456", "--section=junk"]);
    let ddeb_path = ddeb_path.expect("dbgsym option");

    let (mainctrl, mainpkg) = extract_package(&deb_path, DEFAULT_COMPRESSION_EXT);
    let (ddebctrl, ddebpkg) = extract_package(&ddeb_path, DEFAULT_COMPRESSION_EXT);

    let stripped = mainpkg.path().join("usr/bin/example");
    assert!(stripped.exists());

    assert!(!mainpkg.path().join("usr/lib/debug/.build-id").exists());
    assert!(ddebpkg.path().join("usr/lib/debug/.build-id").exists());

    let debug = glob::glob(ddebpkg.path().join("usr/lib/debug/.build-id/*/*.debug").to_str().unwrap())
        .unwrap().flatten().next().expect("can't find gnu-debuglink file in usr/lib/debug/.build-id/");

    assert!(
        debug.exists(),
        "unable to find executable with debug symbols {} for stripped executable {}",
        debug.display(),
        stripped.display()
    );

    let mainctrl = std::fs::read_to_string(mainctrl.path().join("control")).unwrap();
    assert!(mainctrl.contains("Package: example\n"), "{mainctrl}");
    assert!(mainctrl.contains("Version: 0.1.0-456\n"), "{mainctrl}");
    assert!(mainctrl.contains("Section: junk\n"), "{mainctrl}");

    let ddebctrl = std::fs::read_to_string(ddebctrl.path().join("control")).unwrap();
    assert!(ddebctrl.contains("Package: example-dbgsym\n"), "{ddebctrl}");
    assert!(ddebctrl.contains("Version: 0.1.0-456\n"), "{ddebctrl}");
    assert!(ddebctrl.contains("Auto-Built-Package: debug-symbols\n"), "{ddebctrl}");
    assert!(ddebctrl.contains("Section: debug\n"), "{ddebctrl}");
    assert!(ddebctrl.contains("Depends: example (= 0.1.0-456)\n"), "{ddebctrl}");
    assert!(ddebctrl.contains("Description: Debug symbols for example"), "{ddebctrl}");
}

#[test]
#[cfg(feature = "lzma")]
fn run_cargo_deb_command_on_example_dir_with_variant() {
    let args = ["--variant=auto_assets", "--no-strip"];
    let (_bdir, deb_path, _) = cargo_deb("example/Cargo.toml", &args);

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
    assert!(control.contains("Package: example-auto-assets\n"), "Control is: {control:?}");
    assert!(control.contains("Version: 0.1.0-1\n"));
    assert!(control.contains("Section: utils\n"));
    assert!(control.contains("Architecture: "));
    assert!(control.contains("Maintainer: cargo-deb developers <cargo-deb@example.invalid>\n"));

    let ddir = tempfile::tempdir().unwrap();
    assert!(Command::new("tar")
        .arg("xJf")
        .current_dir(ddir.path())
        .arg(ardir.path().join(format!("data.tar.{ext}")))
        .status().unwrap().success());

    assert!(ddir.path().join("var/lib/example/1.txt").exists());
    assert!(ddir.path().join("var/lib/example/2.txt").exists());
    assert!(ddir.path().join("var/lib/example/4.txt").exists());
    assert!(ddir.path().join("usr/share/doc/example-auto-assets/copyright").exists());
    assert!(ddir.path().join("usr/share/doc/example-auto-assets/changelog.Debian.gz").exists());
    assert!(ddir.path().join("usr/bin/example").exists());
}

#[test]
#[cfg(all(feature = "lzma", target_family = "unix"))]
fn run_cargo_deb_command_on_example_dir_with_version() {
    let (_bdir, deb_path, _) = cargo_deb("example/Cargo.toml", &["--deb-version=1my-custom-version", "--maintainer=alternative maintainer"]);

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
    assert!(control.contains("Version: 1my-custom-version\n"));
    assert!(control.contains("Section: utils\n"));
    assert!(control.contains("Architecture: "));
    assert!(control.contains("Maintainer: alternative maintainer\n"));

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
    assert!(
        output.status.success(),
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
