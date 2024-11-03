use crate::util::compress::Compressed;
use crate::CDResult;
use ar::{Builder, Header};
use std::fs;
use std::fs::File;
use std::path::PathBuf;

/// The outermost `ar` archive that contains tarballs inside
pub struct DebArchive {
    out_abspath: PathBuf,
    ar_builder: Builder<File>,
    mtime_timestamp: u64,
}

impl DebArchive {
    pub fn new(out_abspath: PathBuf, mtime_timestamp: u64) -> CDResult<Self> {
        let _ = fs::create_dir_all(out_abspath.parent().ok_or("invalid dir")?);
        let ar_builder = Builder::new(File::create(&out_abspath)?);

        let mut ar = Self {
            out_abspath,
            ar_builder,
            mtime_timestamp,
        };
        ar.add_file("debian-binary".into(), b"2.0\n")?;
        Ok(ar)
    }

    pub fn add_control(&mut self, control_tarball: Compressed) -> CDResult<()> {
        self.add_file(format!("control.tar.{}", control_tarball.extension()), &control_tarball)
    }

    pub fn add_data(&mut self, data_tarball: Compressed) -> CDResult<()> {
        self.add_file(format!("data.tar.{}", data_tarball.extension()), &data_tarball)
    }

    fn add_file(&mut self, dest_path: String, data: &[u8]) -> CDResult<()> {
        let mut header = Header::new(dest_path.into(), data.len() as u64);
        header.set_mode(0o100644); // dpkg uses 100644
        header.set_mtime(self.mtime_timestamp);
        header.set_uid(0);
        header.set_gid(0);
        self.ar_builder.append(&header, data)?;
        Ok(())
    }

    pub fn finish(self) -> CDResult<PathBuf> {
        Ok(self.out_abspath)
    }
}
