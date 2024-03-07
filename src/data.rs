use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::manifest::{Asset, AssetSource, Config, IsBuilt};
use crate::tararchive::Archive;
use md5::Digest;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use zopfli::{BlockType, GzipEncoder, Options};

/// Generates an uncompressed tar archive and hashes of its files
pub fn generate_archive<W: Write>(dest: W, options: &Config, time: u64, rsyncable: bool, listener: &dyn Listener) -> CDResult<(W, HashMap<PathBuf, Digest>)> {
    let mut archive = Archive::new(dest, time);
    let copy_hashes = archive_files(&mut archive, options, rsyncable, listener)?;
    Ok((archive.into_inner()?, copy_hashes))
}

/// Generates compressed changelog file
pub(crate) fn generate_changelog_asset(options: &Config) -> CDResult<Option<Vec<u8>>> {
    if let Some(ref path) = options.changelog {
        let changelog = fs::read(options.path_in_package(path))
            .and_then(|content| {
                // The input is plaintext, but the debian package should contain gzipped one.
                gzipped(&content)
            })
            .map_err(move |e| CargoDebError::IoFile("unable to read changelog file", e, path.into()))?;
        Ok(Some(changelog))
    } else {
        Ok(None)
    }
}

fn append_copyright_metadata(copyright: &mut Vec<u8>, options: &Config) -> Result<(), CargoDebError> {
    writeln!(copyright, "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/")?;
    writeln!(copyright, "Upstream-Name: {}", options.name)?;
    if let Some(source) = options.repository.as_ref().or(options.homepage.as_ref()) {
        writeln!(copyright, "Source: {source}")?;
    }
    writeln!(copyright, "Copyright: {}", options.copyright)?;
    if let Some(ref license) = options.license {
        writeln!(copyright, "License: {license}")?;
    }
    Ok(())
}

/// Generates the copyright file from the license file and adds that to the tar archive.
pub(crate) fn generate_copyright_asset(options: &Config) -> CDResult<Vec<u8>> {
    let mut copyright: Vec<u8> = Vec::new();
    if let Some(ref path) = options.license_file {
        let license_string = fs::read_to_string(options.path_in_package(path))
            .map_err(|e| CargoDebError::IoFile("unable to read license file", e, path.clone()))?;
        if !has_copyright_metadata(&license_string) {
            append_copyright_metadata(&mut copyright, options)?;
        }

        // Skip the first `A` number of lines and then iterate each line after that.
        for line in license_string.lines().skip(options.license_file_skip_lines) {
            // If the line is a space, add a dot, else write the line.
            if line == " " {
                copyright.write_all(b" .\n")?;
            } else {
                copyright.write_all(line.as_bytes())?;
                copyright.write_all(b"\n")?;
            }
        }
    } else {
        append_copyright_metadata(&mut copyright, options)?;
    }

    // Write a copy to the disk for the sake of obtaining a md5sum for the control archive.
    Ok(copyright)
}

fn has_copyright_metadata(file: &str) -> bool {
    file.lines().take(10)
        .any(|l| l.starts_with("License: ") || l.starts_with("Source: ") || l.starts_with("Upstream-Name: ") || l.starts_with("Format: "))
}

/// Compress man pages and other assets per Debian Policy.
///
/// # References
///
/// <https://www.debian.org/doc/debian-policy/ch-docs.html>
/// <https://lintian.debian.org/tags/manpage-not-compressed.html>
pub fn compress_assets(options: &mut Config, listener: &dyn Listener) -> CDResult<()> {
    let mut indices_to_remove = Vec::new();
    let mut new_assets = Vec::new();

    fn needs_compression(path: &str) -> bool {
        !path.ends_with(".gz")
            && (path.starts_with("usr/share/man/")
                || (path.starts_with("usr/share/doc/")
                    && (path.ends_with("/NEWS") || path.ends_with("/changelog")))
                || (path.starts_with("usr/share/info/") && path.ends_with(".info")))
    }

    for (idx, asset) in options.assets.resolved.iter().enumerate() {
        let target_path_str = asset.c.target_path.to_string_lossy();
        if needs_compression(&target_path_str) {
            listener.info(format!("Compressing '{}'", asset.source.path().unwrap_or_else(|| Path::new("-")).display()));
            new_assets.push(Asset::new(
                crate::manifest::AssetSource::Data(gzipped(&asset.source.data()?)?),
                format!("{target_path_str}.gz").into(),
                asset.c.chmod,
                IsBuilt::No,
                false,
            ));

            indices_to_remove.push(idx);
        }
    }

    for idx in indices_to_remove.iter().rev() {
        options.assets.resolved.swap_remove(*idx);
    }

    options.assets.resolved.append(&mut new_assets);

    Ok(())
}

/// Copies all the files to be packaged into the tar archive.
/// Returns MD5 hashes of files copied
fn archive_files<W: Write>(archive: &mut Archive<W>, options: &Config, rsyncable: bool, listener: &dyn Listener) -> CDResult<HashMap<PathBuf, Digest>> {
    let (send, recv) = mpsc::sync_channel(2);
    std::thread::scope(move |s| {
        let num_items = options.assets.resolved.len();
        let hash_thread = s.spawn(move || {
            let mut hashes = HashMap::with_capacity(num_items);
            hashes.extend(recv.into_iter().map(|(path, data)| {
                (path, md5::compute(data))
            }));
            hashes
        });
        let mut archive_data_added = 0;
        let mut prev_is_built = false;
        for asset in &options.assets.resolved {
            let mut log_line = format!("{} -> {}",
                asset.source.path().unwrap_or_else(|| Path::new("-")).display(),
                asset.c.target_path.display()
            );
            if let Some(len) = asset.source.file_size() {
                let (size, unit) = human_size(len);
                use std::fmt::Write;
                let _ = write!(&mut log_line, " ({size}{unit})");
            }
            listener.info(log_line);

            match &asset.source {
                AssetSource::Symlink(source_path) => {
                    let link_name = fs::read_link(source_path)
                        .map_err(|e| CargoDebError::IoFile("symlink asset", e, source_path.clone()))?;
                    archive.symlink(&asset.c.target_path, &link_name)?;
                }
                _ => {
                    let out_data = asset.source.data()?;
                    if rsyncable {
                        if archive_data_added > 1_000_000 || prev_is_built != asset.c.is_built() {
                            archive.flush()?;
                            archive_data_added = 0;
                        }
                        // puts synchronization point between non-code and code assets
                        prev_is_built = asset.c.is_built();
                        archive_data_added += out_data.len();
                    }
                    archive.file(&asset.c.target_path, &out_data, asset.c.chmod)?;
                    send.send((asset.c.target_path.clone(), out_data)).unwrap();
                },
            }
        }
        drop(send);
        Ok(hash_thread.join().unwrap())
    })
}

fn human_size(len: u64) -> (u64, &'static str) {
    if len < 1000 {
        return (len, "B");
    }
    if len < 1_000_000 {
        return ((len + 999) / 1000, "KB");
    }
    ((len + 999_999) / 1_000_000, "MB")
}

fn gzipped(mut content: &[u8]) -> io::Result<Vec<u8>> {
    let mut compressed = Vec::with_capacity(content.len() * 2 / 3);
    let mut encoder = GzipEncoder::new(Options {
        iteration_count: NonZeroU64::new(7).unwrap(),
        ..Options::default()
    }, BlockType::Dynamic, &mut compressed)?;
    io::copy(&mut content, &mut encoder)?;
    encoder.finish()?;
    Ok(compressed)
}
