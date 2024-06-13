use crate::assets::{Asset, Config, IsBuilt};
use crate::compress::gzipped;
use crate::error::CDResult;
use crate::listener::Listener;

/// Compress man pages and other assets per Debian Policy.
///
/// # References
///
/// <https://www.debian.org/doc/debian-policy/ch-docs.html>
/// <https://lintian.debian.org/tags/manpage-not-compressed.html>
pub fn compress_assets(config: &mut Config, listener: &dyn Listener) -> CDResult<()> {
    let mut indices_to_remove = Vec::new();
    let mut new_assets = Vec::new();

    fn needs_compression(path: &str) -> bool {
        !path.ends_with(".gz")
            && (path.starts_with("usr/share/man/")
                || (path.starts_with("usr/share/doc/") && (path.ends_with("/NEWS") || path.ends_with("/changelog")))
                || (path.starts_with("usr/share/info/") && path.ends_with(".info")))
    }

    for (idx, orig_asset) in config.deb.assets.resolved.iter().enumerate() {
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
        config.deb.assets.resolved.swap_remove(*idx);
    }

    config.deb.assets.resolved.append(&mut new_assets);

    Ok(())
}
