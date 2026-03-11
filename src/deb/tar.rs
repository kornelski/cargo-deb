use crate::assets::{Asset, AssetSource};
use crate::error::{CDResult, CargoDebError};
use crate::listener::Listener;
use crate::PackageConfig;
use crate::util::pathbytes::AsUnixPathBytes;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::{fs, io};
use tar::{EntryType, Header as TarHeader};

/// Tarball for control and data files
pub(crate) struct Tarball<W: Write> {
    added_directories: HashSet<Box<Path>>,
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

                let Some(normalized_link_name) = normalize_link_name(&asset.c.target_path, &link_name) else {
                    return Err(CargoDebError::InvalidSymlink(asset.c.target_path.clone(), link_name.clone()));
                };
                
                self.symlink(&asset.c.target_path, &normalized_link_name)?;
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
                self.file(&asset.c.target_path, &out_data, asset.c.chmod.unwrap_or(0o644))?;
            }
        }

        self.tar.into_inner().map_err(|e| CargoDebError::Io(e).context("error while finalizing tar archive"))
    }

    fn directory(&mut self, path: &Path) -> io::Result<()> {
        let mut header = self.header_for_path(path, true)?;
        header.set_mtime(self.time);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_entry_type(EntryType::Directory);
        header.set_cksum();
        self.tar.append(&header, &mut io::empty())
    }

    fn add_parent_directories(&mut self, path: &Path) -> CDResult<()> {
        debug_assert!(path.is_relative());

        let dirs = path.ancestors().skip(1)
            .take_while(|&d| !self.added_directories.contains(d))
            .filter(|&d| !d.as_os_str().is_empty())
            .map(Box::from)
            .collect::<Vec<_>>();

        for directory in dirs.into_iter().rev() {
            if let Err(e) = self.directory(&directory) {
                return Err(CargoDebError::IoFile("Can't add directory to tarball", e, directory.into()));
            }
            self.added_directories.insert(directory);
        }
        Ok(())
    }

    pub(crate) fn file<P: AsRef<Path>>(&mut self, path: P, out_data: &[u8], chmod: u32) -> CDResult<()> {
        self.file_(path.as_ref(), out_data, chmod)
    }

    fn file_(&mut self, path: &Path, out_data: &[u8], chmod: u32) -> CDResult<()> {
        debug_assert!(path.is_relative());
        self.add_parent_directories(path)?;

        let mut header = self.header_for_path(path, false)
            .map_err(|e| CargoDebError::IoFile("Can't set header path", e, path.into()))?;
        header.set_mtime(self.time);
        header.set_mode(chmod);
        header.set_size(out_data.len() as u64);
        header.set_cksum();
        self.tar.append(&header, out_data)
            .map_err(|e| CargoDebError::IoFile("Can't add file to tarball", e, path.into()))?;
        Ok(())
    }

    pub(crate) fn symlink(&mut self, path: &Path, link_name: &Path) -> CDResult<()> {
        debug_assert!(path.is_relative());
        self.add_parent_directories(path.as_ref())?;

        let mut header = self.header_for_path(path, false)
            .map_err(|e| CargoDebError::IoFile("Can't set header path", e, path.into()))?;
        header.set_mtime(self.time);
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name(link_name)
            .map_err(|e| CargoDebError::IoFile("Can't set header link name", e, path.into()))?;
        header.set_cksum();
        self.tar.append(&header, &mut io::empty())
            .map_err(|e| CargoDebError::IoFile("Can't add symlink to tarball", e, path.into()))?;
        Ok(())
    }

    #[inline]
    fn header_for_path(&mut self, path: &Path, is_dir: bool) -> io::Result<TarHeader> {
        debug_assert!(path.is_relative());
        let path_bytes = path.to_bytes();

        let mut header = if path_bytes.len() < 98 {
            TarHeader::new_old()
        } else {
            TarHeader::new_gnu()
        };
        self.set_header_path(&mut header, path_bytes, is_dir)?;
        Ok(header)
    }

    #[inline(never)]
    fn set_header_path(&mut self, header: &mut TarHeader, path_bytes: &[u8], is_dir: bool) -> io::Result<()> {
        debug_assert!(is_dir || path_bytes.last() != Some(&b'/'));
        let needs_slash = is_dir && path_bytes.last() != Some(&b'/');

        const PREFIX: &[u8] = b"./";
        let (prefix, path_slot) = header.as_old_mut().name.split_at_mut(PREFIX.len());
        prefix.copy_from_slice(PREFIX);
        let (path_slot, zero) = path_slot.split_at_mut(path_bytes.len().min(path_slot.len()));
        path_slot.copy_from_slice(&path_bytes[..path_slot.len()]);
        if cfg!(target_os = "windows") {
            for b in path_slot {
                if *b == b'\\' { *b = b'/' }
            }
        }

        if let Some((t, rest)) = zero.split_first_mut() {
            if !needs_slash {
                *t = 0;
                return Ok(());
            }
            if let Some(t2) = rest.first_mut() {
                // Lintian insists on dir paths ending with /, which Rust doesn't
                *t = b'/';
                *t2 = 0;
                return Ok(());
            }
        }

        // GNU long name extension, copied from
        // https://github.com/alexcrichton/tar-rs/blob/a1c3036af48fa02437909112239f0632e4cfcfae/src/builder.rs#L731-L744
        let mut header = TarHeader::new_gnu();
        const LONG_LINK: &[u8] = b"././@LongLink\0";
        header.as_gnu_mut().ok_or(io::ErrorKind::Other)?
            .name[..LONG_LINK.len()].copy_from_slice(LONG_LINK);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        // include \0 in len to be compliant with GNU tar
        let suffix = b"/\0";
        let suffix = if needs_slash { &suffix[..] } else { &suffix[1..] };
        header.set_size((PREFIX.len() + path_bytes.len() + suffix.len()) as u64);
        header.set_entry_type(EntryType::new(b'L'));
        header.set_cksum();
        self.tar.append(&header, PREFIX.chain(path_bytes).chain(suffix))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tar.get_mut().flush()
    }

    pub fn into_inner(self) -> io::Result<W> {
        self.tar.into_inner()
    }
}

fn normalize_link_name(target_path: &Path, link_name: &Path) -> Option<PathBuf> {
    // normalize symlinks according to https://www.debian.org/doc/debian-policy/ch-files.html#symbolic-links 
    // like dh_link https://manpages.debian.org/testing/debhelper/dh_link.1.en.html#DESCRIPTION

    
    let normalized_target_path = join_lexically("/".as_ref(), target_path)?;

    let target_parent = normalized_target_path.parent().expect("the root path is an invalid target");

    let resolved_link = join_lexically(target_parent, link_name)?;

    // normalized_target_path and resolved_link are now absolute and don't contain /./ or /../ components

    let mut target_components = target_parent.components();
    let mut link_components = resolved_link.components();

    if target_components.nth(1) != link_components.nth(1) {
        // the paths differ in the top level folder (after the root dir) so the link must be absolute
        return Some(resolved_link);
    }

    let mut link = PathBuf::new();

    loop {
        let next_target = target_components.next();
        let next_link = link_components.next();

        match (next_target, next_link) {
            (None, None) => break Some(link),
            (None, Some(comp)) => {
                link.push(comp);
                link.extend(link_components);
                break Some(link);
                
            },
            (Some(_), None) => {
                for _ in 0..=target_components.count() {
                    link = AsRef::<Path>::as_ref("..").join(link)
                }
                break Some(link);
            },
            (Some(l), Some(r)) => {
                if l == r {
                    continue;
                }

                for _ in 0..=(&mut target_components).count()  {
                    link = AsRef::<Path>::as_ref("..").join(link)
                }

                link.push(r);
            },
        }
    }
}

// Join the two paths while normalizing them lexically, so that the final path contains no /./ or /../ components
// Assumes that base is already lexically normalized.
// returns None if we at some point we attempted to ascend beyond the first component of base
fn join_lexically(base: &Path, adjoint_path: &Path) -> Option<PathBuf> {
    let mut resolved_link = base.to_path_buf();
    for comp in adjoint_path.components() {
        match comp {
            Component::Prefix(_) => unreachable!(),
            Component::RootDir => {
                resolved_link = PathBuf::from("/");
            },
            Component::CurDir => {},
            Component::ParentDir => {
                
                if !resolved_link.pop() {
                    return None;
                }
            },
            Component::Normal(os_str) => {
                resolved_link.push(os_str);
            },
        }
    }
    Some(resolved_link)
}

#[test]
fn normalized_links() {
    let examples = [
        ("usr/lib/foo", "/usr/share/bar", Some("../share/bar")),
        ("usr/lib/foo", "/usr/share/./bar", Some("../share/bar")),
        ("usr/lib/foo", "/usr/share/foo/../bar", Some("../share/bar")),
        ("usr/lib/foo", "/var/lib/foo/../bar", Some("/var/lib/bar")),
        ("usr/lib/foo", "/var/lib/foo/./bar", Some("/var/lib/foo/bar")),
        ("var/run", "/run", Some("/run")),
        ("usr/share/foo", "../../../var/lib/baz", None),
        ("usr/share/foo", "../../var/lib/baz", Some("/var/lib/baz")),
        ("usr/share/foo", "../../usr/lib/baz", Some("../lib/baz")),
    ];

    for (target, link_name, result) in examples {
        assert_eq!(normalize_link_name(target.as_ref(), link_name.as_ref()).as_deref(), result.map(AsRef::<Path>::as_ref), "{target} -> {link_name} should normalize to {result:?}")
    }
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

#[cfg(test)]
mod tests {
    use super::Tarball;
    use std::{io::{Cursor, Read}, path::Path};
    use tar::{Archive, EntryType};

    struct ExpectedEntry<'a> {
        path: &'a str,
        entry_type: EntryType,
        mode: u32,
        check: Option<Box<dyn Fn(&mut tar::Entry<Cursor<Vec<u8>>>) + 'a>>,
    }

    impl<'a> ExpectedEntry<'a> {
        fn with_check<F>(mut self, check: F) -> Self
            where F: Fn(&mut tar::Entry<Cursor<Vec<u8>>>) + 'a
        {
            self.check = Some(Box::new(check));
            self
        }
    }

    fn expected_entry(path: &str, entry_type: EntryType, mode: u32) -> ExpectedEntry<'_> {
        ExpectedEntry { path, entry_type, mode, check: None }
    }

    fn check_tarball_content(tarball: Vec<u8>, expected_entries: &[ExpectedEntry]) {
        let cursor = Cursor::new(tarball);
        let mut archive = Archive::new(cursor);
        let mut entries = archive.entries().unwrap();
        let mut expected_entries = expected_entries.iter();
        loop {
            let (entry_result, expected_entry) = match (entries.next(), expected_entries.next()) {
                (Some(entry_result), Some(expected_entry)) => (entry_result, expected_entry),
                (None, None) => break,
                _ => panic!("mismatched number of entries"),
            };
            let mut entry = entry_result.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            let entry_type = entry.header().entry_type();
            let mode = entry.header().mode().unwrap();
            let mtime = entry.header().mtime().unwrap();
            assert_eq!(path.strip_prefix("./").unwrap(), expected_entry.path);
            assert_eq!(entry_type, expected_entry.entry_type);
            assert_eq!(mode, expected_entry.mode);
            assert_eq!(mtime, 1234567890);
            if let Some(check) = &expected_entry.check {
                check(&mut entry);
            }
        }
    }

    #[test]
    fn basic() {
        let buffer = Vec::new();
        let mut tarball = Tarball::new(buffer, 1234567890);
        let file_content = b"Hello, world!";
        tarball.file("test/file.txt", file_content, 0o644).unwrap();
        let script_content = b"#!/bin/bash\necho 'test'";
        tarball.file("usr/bin/script", script_content, 0o755).unwrap();
        tarball.symlink(Path::new("usr/bin/link"), Path::new("script")).unwrap();

        let buffer = tarball.into_inner().unwrap();
        check_tarball_content(buffer, &[
            expected_entry("test/", EntryType::Directory, 0o755),
            expected_entry("test/file.txt", EntryType::Regular, 0o644).with_check(|entry| {
                let mut content = Vec::new();
                entry.read_to_end(&mut content).unwrap();
                assert_eq!(content, file_content);
            }),
            expected_entry("usr/", EntryType::Directory, 0o755),
            expected_entry("usr/bin/", EntryType::Directory, 0o755),
            expected_entry("usr/bin/script", EntryType::Regular, 0o755).with_check(|entry| {
                let mut content = Vec::new();
                entry.read_to_end(&mut content).unwrap();
                assert_eq!(content, script_content);
            }),
            expected_entry("usr/bin/link", EntryType::Symlink, 0o777).with_check(|entry| {
                let link_name = entry.header().link_name().unwrap().unwrap();
                assert_eq!(link_name.to_string_lossy(), "script");
            }),
        ]);
    }

    #[test]
    fn long_path() {
        let buffer = Vec::new();
        let mut tarball = Tarball::new(buffer, 1234567890);

        tarball.file("a.txt", b"start", 0o644).unwrap();
        let level = "long/";
        let deep_path = level.repeat(25) + "file.txt";
        tarball.file(&deep_path, b"long path", 0o644).unwrap();
        let long_filename = "very_".repeat(25) + "long_filename.txt";
        tarball.file(&long_filename, b"long filename", 0o644).unwrap();
        tarball.file("b.txt", b"end", 0o644).unwrap();
        let buffer = tarball.into_inner().unwrap();

        let mut expected_entries = vec![expected_entry("a.txt", EntryType::Regular, 0o644)];
        expected_entries.extend((1..=25).map(|i| expected_entry(&deep_path[..i * level.len()], EntryType::Directory, 0o755)));
        expected_entries.extend([
            expected_entry(&deep_path, EntryType::Regular, 0o644),
            expected_entry(&long_filename, EntryType::Regular, 0o644),
            expected_entry("b.txt", EntryType::Regular, 0o644),
        ]);
        check_tarball_content(buffer, &expected_entries);
    }
}
