use crate::assets::Config;
use crate::CDResult;
use crate::Package;
use ar::{Builder, Header};
use std::fs;
use std::fs::File;
use std::path::PathBuf;

pub struct Archive {
    out_abspath: PathBuf,
    ar_builder: Builder<File>,
}

impl Archive {
    pub fn new(config: &Config) -> CDResult<Self> {
        let out_filename = format!("{}_{}_{}.deb", config.deb.deb_name, config.deb.deb_version, config.deb.architecture);
        let out_abspath = config.deb_output_path(&out_filename);
        {
            let deb_dir = out_abspath.parent().ok_or("invalid dir")?;
            let _ = fs::create_dir_all(deb_dir);
        }
        let ar_builder = Builder::new(File::create(&out_abspath)?);

        Ok(Archive {
            out_abspath,
            ar_builder,
        })
    }

    pub(crate) fn filename_glob(deb: &Package) -> String {
        format!("{}_*_{}.deb", deb.deb_name, deb.architecture)
    }

    pub fn add_data(&mut self, dest_path: String, mtime_timestamp: u64, data: &[u8]) -> CDResult<()> {
        let mut header = Header::new(dest_path.into(), data.len() as u64);
        header.set_mode(0o100644); // dpkg uses 100644
        header.set_mtime(mtime_timestamp);
        header.set_uid(0);
        header.set_gid(0);
        self.ar_builder.append(&header, data)?;
        Ok(())
    }

    pub fn finish(self) -> CDResult<PathBuf> {
        Ok(self.out_abspath)
    }
}
