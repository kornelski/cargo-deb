#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub trait AsUnixPathBytes {
    fn to_bytes(&self) -> &[u8];
}

impl AsUnixPathBytes for Path {
    #[cfg(not(unix))]
    fn to_bytes(&self) -> &[u8] {
        self.to_str().unwrap().as_bytes()
    }

    #[cfg(unix)]
    fn to_bytes(&self) -> &[u8] {
        self.as_os_str().as_bytes()
    }
}
