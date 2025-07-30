use crate::error::{CDResult, CargoDebError};
use std::path::Path;
use std::process::Command;

const DPKG_SHLIBDEPS_COMMAND: &str = "dpkg-shlibdeps";

/// Resolves the dependencies based on the output of dpkg-shlibdeps on the binary.
pub(crate) fn resolve_with_dpkg(path: &Path, lib_dir_search_paths: &[&Path]) -> CDResult<Vec<String>> {
    let temp_folder = tempfile::tempdir().map_err(CargoDebError::Io)?;
    let debian_folder = temp_folder.path().join("debian");
    let control_file_path = debian_folder.join("control");
    let _ = std::fs::create_dir_all(&debian_folder);
    // dpkg-shlibdeps requires a (possibly empty) debian/control file to exist in its working
    // directory. The executable location doesn't matter.
    let _ = std::fs::File::create(&control_file_path)
        .map_err(|e| CargoDebError::IoFile("can't make temp file", e, control_file_path))?;

    let mut cmd = Command::new(DPKG_SHLIBDEPS_COMMAND);
    // Print result to stdout instead of a file.
    cmd.arg("-O");
    // determine library search path from target
    for dir in lib_dir_search_paths {
        debug_assert!(dir.exists());
        cmd.args(["-l".as_ref(), dir.as_os_str()]);
    }
    let output = cmd
        .arg(path)
        .current_dir(temp_folder.path())
        .output()
        .map_err(|e| CargoDebError::CommandFailed(e, DPKG_SHLIBDEPS_COMMAND.into()))?;
    if !output.status.success() {
        use std::fmt::Write;
        let mut args = String::new();
        for lib_dir_search_path in lib_dir_search_paths {
            let _ = write!(&mut args, "-l {} ", lib_dir_search_path.display());
        }
        let _ = write!(&mut args, "{}", path.display());
        return Err(CargoDebError::CommandError(
            DPKG_SHLIBDEPS_COMMAND,
            args,
            output.stderr,
        ));
    }

    log::debug!("dpkg-shlibdeps for {}: {}", path.display(), String::from_utf8_lossy(&output.stdout));

    let deps = output.stdout.as_slice().split(|&c| c == b'\n')
        .find_map(|line| line.strip_prefix(b"shlibs:Depends="))
        .ok_or(CargoDebError::Str("Failed to find dependency specification."))?
        .split(|&c| c == b',')
        .filter_map(|dep| std::str::from_utf8(dep).ok())
        .map(|dep| dep.trim_matches(|c: char| c.is_ascii_whitespace()))
        // libgcc guaranteed by LSB to always be present
        .filter(|dep| !dep.starts_with("libgcc-") && !dep.starts_with("libgcc1"))
        .map(|dep| dep.to_string())
        .collect();

    Ok(deps)
}

#[test]
#[cfg(target_os = "linux")]
fn resolve_test() {
    let exe = std::env::current_exe().unwrap();
    let deps = resolve_with_dpkg(&exe, &[]).unwrap();
    assert!(deps.iter().any(|d| d.starts_with("libc")));
    assert!(!deps.iter().any(|d| d.starts_with("libgcc")), "{deps:?}");
}
