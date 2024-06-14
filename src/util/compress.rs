use crate::error::{CDResult, CargoDebError};
use std::io;
use std::io::{BufWriter, Read};
use std::num::NonZeroU64;
#[cfg(feature = "lzma")]
use std::num::NonZeroUsize;
use std::ops;
use std::process::{Child, ChildStdin};
use std::process::{Command, Stdio};
use zopfli::{BlockType, GzipEncoder, Options};

pub struct CompressConfig {
    pub fast: bool,
    pub compress_type: Format,
    pub compress_system: bool,
    pub rsyncable: bool,
}

#[derive(Clone, Copy)]
pub enum Format {
    Xz,
    Gzip,
}

impl Format {
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Xz => "xz",
            Self::Gzip => "gz",
        }
    }

    fn program(self) -> &'static str {
        match self {
            Self::Xz => "xz",
            Self::Gzip => "gzip",
        }
    }

    fn level(self, fast: bool) -> u32 {
        match self {
            Self::Xz => if fast { 1 } else { 6 },
            Self::Gzip => if fast { 1 } else { 9 },
        }
    }
}

enum Writer {
    #[cfg(feature = "lzma")]
    Xz(xz2::write::XzEncoder<Vec<u8>>),
    Gz(flate2::write::GzEncoder<Vec<u8>>),
    ZopfliGz(BufWriter<GzipEncoder<Vec<u8>>>),
    StdIn {
        compress_format: Format,
        child: Child,
        handle: std::thread::JoinHandle<io::Result<Vec<u8>>>,
        stdin: BufWriter<ChildStdin>,
    },
}

impl Writer {
    fn finish(self) -> io::Result<Compressed> {
        match self {
            #[cfg(feature = "lzma")]
            Self::Xz(w) => w.finish().map(|data| Compressed { compress_format: Format::Xz, data }),
            Self::StdIn {
                compress_format,
                mut child,
                handle,
                stdin,
            } => {
                drop(stdin);
                child.wait()?;
                handle.join().unwrap().map(|data| Compressed { compress_format, data })
            }
            Self::Gz(w) => w.finish().map(|data| Compressed { compress_format: Format::Gzip, data }),
            Self::ZopfliGz(w) => w.into_inner()?.finish().map(|data| Compressed { compress_format: Format::Gzip, data }),
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
            Writer::ZopfliGz(w) => w.flush(),
            Writer::StdIn { stdin, .. } => stdin.flush(),
        }
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = match &mut self.writer {
            #[cfg(feature = "lzma")]
            Writer::Xz(w) => w.write(buf),
            Writer::Gz(w) => w.write(buf),
            Writer::ZopfliGz(w) => w.write(buf),
            Writer::StdIn { stdin, .. } => stdin.write(buf),
        }?;
        self.uncompressed_size += len;
        Ok(len)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match &mut self.writer {
            #[cfg(feature = "lzma")]
            Writer::Xz(w) => w.write_all(buf),
            Writer::Gz(w) => w.write_all(buf),
            Writer::ZopfliGz(w) => w.write_all(buf),
            Writer::StdIn { stdin, .. } => stdin.write_all(buf),
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
    compress_format: Format,
    data: Vec<u8>,
}

impl Compressed {
    #[must_use]
    pub fn extension(&self) -> &'static str {
        self.compress_format.extension()
    }
}

impl ops::Deref for Compressed {
    type Target = Vec<u8>;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

fn system_compressor(compress_format: Format, fast: bool) -> CDResult<Compressor> {
    let mut child = Command::new(compress_format.program())
        .arg(format!("-{}", compress_format.level(fast)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| CargoDebError::CommandFailed(e, compress_format.program()))?;
    let mut stdout = child.stdout.take().unwrap();

    let handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });

    let stdin = BufWriter::with_capacity(1<<16, child.stdin.take().unwrap());
    Ok(Compressor::new(Writer::StdIn { compress_format, child, handle, stdin }))
}

pub fn select_compressor(fast: bool, compress_format: Format, use_system: bool) -> CDResult<Compressor> {
    if use_system {
        return system_compressor(compress_format, fast);
    }

    match compress_format {
        #[cfg(feature = "lzma")]
        Format::Xz => {
            // Compression level 6 is a good trade off between size and [ridiculously] long compression time
            let encoder = xz2::stream::MtStreamBuilder::new()
                .threads(std::thread::available_parallelism().unwrap_or(NonZeroUsize::new(1).unwrap()).get() as u32)
                .preset(compress_format.level(fast))
                .encoder()
                .map_err(CargoDebError::LzmaCompressionError)?;

            let writer = xz2::write::XzEncoder::new_stream(Vec::new(), encoder);
            Ok(Compressor::new(Writer::Xz(writer)))
        },
        #[cfg(not(feature = "lzma"))]
        Format::Xz => system_compressor(compress_format, fast),
        Format::Gzip => {
            use flate2::write::GzEncoder;
            use flate2::Compression;

            let writer = if !fast {
                let inner_writer = GzipEncoder::new_buffered(Options {
                    iteration_count: NonZeroU64::new(7).unwrap(),
                    ..Options::default()
                }, BlockType::Dynamic, Vec::new()).unwrap();
                Writer::ZopfliGz(inner_writer)
            } else {
                let inner_writer = GzEncoder::new(Vec::new(), Compression::new(compress_format.level(fast)));
                Writer::Gz(inner_writer)
            };
            Ok(Compressor::new(writer))
        },
    }
}

pub(crate) fn gzipped(mut content: &[u8]) -> io::Result<Vec<u8>> {
    let mut compressed = Vec::with_capacity(content.len() * 2 / 3);
    let mut encoder = GzipEncoder::new(
        Options {
            iteration_count: NonZeroU64::new(7).unwrap(),
            ..Options::default()
        },
        BlockType::Dynamic,
        &mut compressed,
    )?;
    io::copy(&mut content, &mut encoder)?;
    encoder.finish()?;
    Ok(compressed)
}
