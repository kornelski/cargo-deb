use crate::assets::{Asset, AssetSource, Config, IsBuilt};
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::tararchive::Archive;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use zopfli::{BlockType, GzipEncoder, Options};

/// Generates an uncompressed tar archive and hashes of its files
pub fn generate_archive<W: Write>(dest: W, options: &Config, time: u64, rsyncable: bool, listener: &dyn Listener) -> CDResult<(W, HashMap<PathBuf, [u8; 32]>)> {
    let mut archive = Archive::new(dest, time);
    let copy_hashes = archive_files(&mut archive, options, rsyncable, listener)?;
    Ok((archive.into_inner()?, copy_hashes))
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
                || (path.starts_with("usr/share/doc/") && (path.ends_with("/NEWS") || path.ends_with("/changelog")))
                || (path.starts_with("usr/share/info/") && path.ends_with(".info")))
    }

    for (idx, orig_asset) in options.deb.assets.resolved.iter().enumerate() {
        if !orig_asset.c.target_path.starts_with("usr") {
            continue;
        }
        let target_path_str = orig_asset.c.target_path.to_string_lossy();
        if needs_compression(&target_path_str) {
            debug_assert!(!orig_asset.c.is_built());

            let mut new_path = target_path_str.into_owned();
            new_path.push_str(".gz");
            listener.info(format!("Compressing '{new_path}'"));
            new_assets.push(Asset::new(
                crate::assets::AssetSource::Data(gzipped(&orig_asset.source.data()?)?),
                new_path.into(),
                orig_asset.c.chmod,
                IsBuilt::No,
                false,
            ).processed("compressed",
                orig_asset.source.path().unwrap_or(&orig_asset.c.target_path).to_path_buf()
            ));
            indices_to_remove.push(idx);
        }
    }

    for idx in indices_to_remove.iter().rev() {
        options.deb.assets.resolved.swap_remove(*idx);
    }

    options.deb.assets.resolved.append(&mut new_assets);

    Ok(())
}

/// Copies all the files to be packaged into the tar archive.
/// Returns MD5 hashes of files copied
fn archive_files<W: Write>(archive: &mut Archive<W>, options: &Config, rsyncable: bool, listener: &dyn Listener) -> CDResult<HashMap<PathBuf, [u8; 32]>> {
    let (send, recv) = mpsc::sync_channel(2);
    std::thread::scope(move |s| {
        let num_items = options.deb.assets.resolved.len();
        let hash_thread = s.spawn(move || {
            let mut hashes = HashMap::with_capacity(num_items);
            hashes.extend(recv.into_iter().map(|(path, data)| {
                (path, Sha256::digest(data).into())
            }));
            hashes
        });
        let mut archive_data_added = 0;
        let mut prev_is_built = false;

        debug_assert!(options.deb.assets.unresolved.is_empty());
        for asset in &options.deb.assets.resolved {
            let mut log_line = format!("{} {}-> {}",
                asset.processed_from.as_ref().and_then(|p| p.original_path.as_deref())
                    .or(asset.source.path())
                    .unwrap_or_else(|| Path::new("-")).display(),
                asset.processed_from.as_ref().map(|p| p.action).unwrap_or_default(),
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

pub(crate) fn gzipped(mut content: &[u8]) -> io::Result<Vec<u8>> {
    let mut compressed = Vec::with_capacity(content.len() * 2 / 3);
    let mut encoder = GzipEncoder::new(Options {
        iteration_count: NonZeroU64::new(7).unwrap(),
        ..Options::default()
    }, BlockType::Dynamic, &mut compressed)?;
    io::copy(&mut content, &mut encoder)?;
    encoder.finish()?;
    Ok(compressed)
}
