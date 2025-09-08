use crate::assets::{Asset, AssetSource};
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::PackageConfig;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::{fs, io};
use tar::{EntryType, Header as TarHeader};

/// Tarball for control and data files
pub(crate) struct Tarball<W: Write> {
    added_directories: HashSet<PathBuf>,
    time: u64,
    tar: tar::Builder<W>,
}

impl<W: Write> Tarball<W> {
    pub fn new(dest: W, time: u64) -> Self {
        Self {
            added_directories: HashSet::new(),
            time,
            tar: tar::Builder::new(dest),
        }
    }

    /// Copies all the files to be packaged into the tar archive.
    pub fn archive_files(mut self, package_deb: &PackageConfig, rsyncable: bool, listener: &dyn Listener) -> CDResult<W> {
        let mut archive_data_added = 0;
        let mut prev_is_built = false;
        let log_display_base_dir = std::env::current_dir().unwrap_or_default();

        debug_assert!(package_deb.assets.unresolved.is_empty());
        for asset in &package_deb.assets.resolved {
            log_asset(asset, &log_display_base_dir, listener);

            if let AssetSource::Symlink(source_path) = &asset.source {
                let link_name = fs::read_link(source_path)
                    .map_err(|e| CargoDebError::IoFile("Symlink asset", e, source_path.clone()))?;
                self.symlink(&asset.c.target_path, &link_name)?;
            } else {
                let out_data = asset.source.data()?;
                if rsyncable {
                    if archive_data_added > 1_000_000 || prev_is_built != asset.c.is_built() {
                        self.flush().map_err(|e| CargoDebError::Io(e).context("error while writing tar archive"))?;
                        archive_data_added = 0;
                    }
                    // puts synchronization point between non-code and code assets
                    prev_is_built = asset.c.is_built();
                    archive_data_added += out_data.len();
                }
                self.file(&asset.c.target_path, &out_data, asset.c.chmod)?;
            }
        }

        self.tar.into_inner().map_err(|e| CargoDebError::Io(e).context("error while finalizing tar archive"))
    }

    fn directory(&mut self, path: &Path) -> io::Result<()> {
        let mut header = TarHeader::new_gnu();
        header.set_mtime(self.time);
        header.set_size(0);
        header.set_mode(0o755);
        // Lintian insists on dir paths ending with /, which Rust doesn't
        let mut path_str = path.to_string_lossy().to_string();
        if !path_str.ends_with('/') {
            path_str += "/";
        }
        set_header_path(&mut header, path_str.as_ref())?;
        header.set_entry_type(EntryType::Directory);
        header.set_cksum();
        self.tar.append(&header, &mut io::empty())
    }

    fn add_parent_directories(&mut self, path: &Path) -> CDResult<()> {
        // Append each of the directories found in the file's pathname to the archive before adding the file
        // For each directory pathname found, attempt to add it to the list of directories
        let asset_relative_dir = Path::new(".").join(path.parent().ok_or("invalid asset path")?);
        let mut directory = PathBuf::new();
        for comp in asset_relative_dir.components() {
            match comp {
                Component::CurDir if !crate::TAR_REJECTS_CUR_DIR => directory.push("."),
                Component::Normal(c) => directory.push(c),
                _ => continue,
            }
            if !self.added_directories.contains(&directory) {
                self.added_directories.insert(directory.clone());
                self.directory(&directory)
                    .map_err(|e| CargoDebError::IoFile("Can't add directory to tarball", e, directory.clone()))?;
            }
        }
        Ok(())
    }

    pub(crate) fn file<P: AsRef<Path>>(&mut self, path: P, out_data: &[u8], chmod: u32) -> CDResult<()> {
        self.file_(path.as_ref(), out_data, chmod)
    }

    fn file_(&mut self, path: &Path, out_data: &[u8], chmod: u32) -> CDResult<()> {
        self.add_parent_directories(path)?;

        let mut header = TarHeader::new_gnu();
        header.set_mtime(self.time);
        header.set_mode(chmod);
        header.set_size(out_data.len() as u64);
        set_header_path(&mut header, path)
            .map_err(|e| CargoDebError::IoFile("Can't set header path", e, path.into()))?;
        header.set_cksum();
        self.tar.append(&header, out_data)
            .map_err(|e| CargoDebError::IoFile("Can't add file to tarball", e, path.into()))?;
        Ok(())
    }

    pub(crate) fn symlink(&mut self, path: &Path, link_name: &Path) -> CDResult<()> {
        self.add_parent_directories(path.as_ref())?;

        let mut header = TarHeader::new_gnu();
        header.set_mtime(self.time);
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        set_header_path(&mut header, path)
            .map_err(|e| CargoDebError::IoFile("Can't set header path", e, path.into()))?;
        header.set_link_name(link_name)
            .map_err(|e| CargoDebError::IoFile("Can't set header link name", e, path.into()))?;
        header.set_cksum();
        self.tar.append(&header, &mut io::empty())
            .map_err(|e| CargoDebError::IoFile("Can't add symlink to tarball", e, path.into()))?;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tar.get_mut().flush()
    }

    pub fn into_inner(self) -> io::Result<W> {
        self.tar.into_inner()
    }
}

fn set_header_path(header: &mut TarHeader, path: &Path) -> io::Result<()> {
    const PREFIX: &[u8] = b"./";
    let header = header.as_old_mut();
    let slot = &mut header.name;
    let bytes = path.as_os_str().as_encoded_bytes();
    if slot.len() < bytes.len() + PREFIX.len() {
        return Err(io::Error::other("Path too long"));
    }
    let (prefix, rest) = slot.split_at_mut(PREFIX.len());
    prefix.copy_from_slice(PREFIX);
    rest[..bytes.len()].copy_from_slice(bytes);
    if cfg!(target_os = "windows") {
        rest.iter_mut().for_each(|b| if *b == b'\\' { *b = b'/' });
    }
    if rest.len() < bytes.len() {
        rest[bytes.len()] = 0;
    }
    Ok(())
}

fn log_asset(asset: &Asset, log_display_base_dir: &Path, listener: &dyn Listener) {
    let operation = if let AssetSource::Symlink(_) = &asset.source {
        "Linking"
    } else {
        "Adding"
    };
    let mut log_line = format!("'{}' {}-> {}",
        asset.processed_from.as_ref().and_then(|p| p.original_path.as_deref()).or(asset.source.path())
            .map(|p| p.strip_prefix(log_display_base_dir).unwrap_or(p))
            .unwrap_or_else(|| Path::new("-")).display(),
        asset.processed_from.as_ref().map(|p| p.action).unwrap_or_default(),
        asset.c.target_path.display()
    );
    if let Some(len) = asset.source.file_size() {
        let (size, unit) = human_size(len);
        use std::fmt::Write;
        let _ = write!(&mut log_line, " ({size}{unit})");
    }
    listener.progress(operation, log_line);
}

fn human_size(len: u64) -> (u64, &'static str) {
    if len < 1000 {
        return (len, "B");
    }
    if len < 1_000_000 {
        return (len.div_ceil(1000), "KB");
    }
    (len.div_ceil(1_000_000), "MB")
}
