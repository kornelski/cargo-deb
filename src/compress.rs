use crate::error::*;
use std::io::{Read, Write};
use std::ops;
use std::process::{Command, Stdio};

pub enum Compressed {
    Gz(Vec<u8>),
    Xz(Vec<u8>),
}

impl ops::Deref for Compressed {
    type Target = Vec<u8>;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Gz(data) |
            Self::Xz(data) => data,
        }
    }
}

impl Compressed {
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Gz(_) => "gz",
            Self::Xz(_) => "xz",
        }
    }
}

fn system_xz(data: &[u8], fast: bool) -> CDResult<Compressed> {
    let mut child = Command::new("xz")
        .arg(if fast { "-1" } else { "-6" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| CargoDebError::CommandFailed(e, "xz"))?;
    let mut stdout = child.stdout.take().unwrap();

    let capacity = data.len() / 2;
    let t = std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(capacity);
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    child.stdin.take().unwrap().write_all(data)?; // This has to close stdin
    Ok(Compressed::Xz(t.join().unwrap()?))
}

/// Compresses data using the [native Rust implementation of Zopfli](https://github.com/carols10cents/zopfli).
#[cfg(not(feature = "lzma"))]
pub fn xz_or_gz(data: &[u8], fast: bool, with_system_xz: bool) -> CDResult<Compressed> {
    match system_xz(data, fast) {
        Ok(compressed) => return Ok(compressed),
        Err(err) if with_system_xz => return Err(err),
        Err(err) => {
            log::debug!("couldn't use system xz: {}", err);
            // not explicitly enabled
        },
    };

    use zopfli::{Format, Options};

    // Compressed data is typically half to a third the original size
    let mut compressed = Vec::with_capacity(data.len() >> 1);
    zopfli::compress(&Options::default(), &Format::Gzip, data, &mut compressed)?;

    Ok(Compressed::Gz(compressed))
}

/// Compresses data using the xz2 library
#[cfg(feature = "lzma")]
pub fn xz_or_gz(data: &[u8], fast: bool, with_system_xz: bool) -> CDResult<Compressed> {
    if with_system_xz {
        return system_xz(data, fast);
    }

    use xz2::stream;
    use xz2::write::XzEncoder;

    // Compressed data is typically half to a third the original size
    let buf = Vec::with_capacity(data.len() >> 1);

    // Compression level 6 is a good trade off between size and [ridiculously] long compression time
    let encoder = stream::MtStreamBuilder::new()
        .threads(num_cpus::get() as u32)
        .preset(if fast { 1 } else { 6 })
        .encoder()
        .map_err(CargoDebError::LzmaCompressionError)?;

    let mut writer = XzEncoder::new_stream(buf, encoder);
    writer.write_all(data).map_err(CargoDebError::Io)?;

    let compressed = writer.finish().map_err(CargoDebError::Io)?;

    Ok(Compressed::Xz(compressed))
}
