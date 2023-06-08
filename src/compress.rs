use crate::error::*;
use std::io::{Read, BufWriter};
use std::io;
use std::ops;
use std::process::{ChildStdin, Child};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Default)]
pub enum CompressType {
    #[default]
    Xz,
    Gzip,
}

impl CompressType {
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Xz => "xz",
            Self::Gzip => "gz",
        }
    }

    fn program(&self) -> &'static str {
        match self {
            Self::Xz => "xz",
            Self::Gzip => "gzip",
        }
    }
}

enum Writer {
    #[cfg(feature = "lzma")]
    Xz(xz2::write::XzEncoder<Vec<u8>>),
    Gz(flate2::write::GzEncoder<Vec<u8>>),
    StdIn {
        compress_type: CompressType,
        child: Child, 
        handle: std::thread::JoinHandle<io::Result<Vec<u8>>>,
        stdin: BufWriter<ChildStdin>
    },
}

impl Writer {
    fn finish(self) -> io::Result<Compressed> {
        match self {
            Self::Xz(w) => w.finish().map(|data| Compressed {compress_type: CompressType::Xz, data}),
            Self::StdIn{compress_type, mut child, handle: join_handle, stdin} => {
                drop(stdin);
                child.wait()?;
                join_handle.join().unwrap().map(|data| Compressed {compress_type, data})
            }
            Self::Gz(w) => w.finish().map(|data| Compressed { compress_type: CompressType::Gzip, data }),   
        }
    }
}

pub struct Compressor {
    writer: Writer,
    pub uncompressed_size: usize,
}

impl io::Write for Compressor {
    fn flush(&mut self) -> io::Result<()> {
        match &mut self.writer {
            #[cfg(feature = "lzma")]
            Writer::Xz(w) => w.flush(),
            Writer::Gz(w) => w.flush(),
            Writer::StdIn{stdin, ..} => stdin.flush(),
        }
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = match &mut self.writer {
            #[cfg(feature = "lzma")]
            Writer::Xz(w) => w.write(buf),
            Writer::Gz(w) => w.write(buf),
            Writer::StdIn{stdin, ..} =>stdin.write(buf),
        }?;
        self.uncompressed_size += len;
        Ok(len)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match &mut self.writer {
            #[cfg(feature = "lzma")]
            Writer::Xz(w) => w.write_all(buf),
            Writer::Gz(w) => w.write_all(buf),
            Writer::StdIn{stdin, ..} => stdin.write_all(buf),
        }?;
        self.uncompressed_size += buf.len();
        Ok(())
    }
}

impl Compressor {
    fn new(writer: Writer) -> Self {
        Self {
            writer,
            uncompressed_size: 0,
        }
    }

    pub fn finish(self) -> CDResult<Compressed> {
        self.writer.finish().map_err(From::from)
    }
}

pub struct Compressed {
    compress_type: CompressType,
    data: Vec<u8>,
}

impl Compressed {
    pub fn extension(&self) -> &'static str {
        self.compress_type.extension()
    }
}

impl ops::Deref for Compressed {
    type Target = Vec<u8>;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

fn system_compressor(compress_type: CompressType, fast: bool) -> CDResult<Compressor> {
    let mut child = Command::new(compress_type.program())
        .arg(if fast { "-1" } else { "-6" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| CargoDebError::CommandFailed(e, compress_type.program()))?;
    let mut stdout = child.stdout.take().unwrap();

    let handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });

    let stdin = BufWriter::with_capacity(1<<16, child.stdin.take().unwrap());
    Ok(Compressor::new(Writer::StdIn{ compress_type, child, handle, stdin }))
}


pub fn select_compressor(fast: bool, compress_type: CompressType, use_system: bool) -> CDResult<Compressor> {
    if use_system {
        return system_compressor(compress_type, fast);
    }

    match compress_type {
        #[cfg(feature = "lzma")]
        CompressType::Xz => {
            // Compression level 6 is a good trade off between size and [ridiculously] long compression time
            let encoder = xz2::stream::MtStreamBuilder::new()
                .threads(num_cpus::get() as u32)
                .preset(if fast { 1 } else { 6 })
                .encoder()
                .map_err(CargoDebError::LzmaCompressionError)?;
        
            let writer = xz2::write::XzEncoder::new_stream(Vec::new(), encoder);
            Ok(Compressor::new(Writer::Xz(writer)))
        }
        #[cfg(not(feature = "lzma"))]
        CompressType::Xz => system_compressor(compress_type, fast),
        CompressType::Gzip => {
            use flate2::Compression;
            use flate2::write::GzEncoder;
        
            let writer = GzEncoder::new(Vec::new(), if fast { Compression::fast() } else { Compression::best() });
            Ok(Compressor::new(Writer::Gz(writer)))
        }
    }
}
