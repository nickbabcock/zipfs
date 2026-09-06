//! A decompressor positioned somewhere inside one entry.
//!
//! Entries cannot be seeked, so a decoder always starts at offset zero and only
//! moves forward. Skipping ahead means decompressing and discarding, which is
//! why the checksum can be kept without any bookkeeping: every byte the codec
//! produces is hashed, whether it is delivered or thrown away, so reaching the
//! declared size proves the checksum covers the whole entry.

use crate::codec::{Codec, Decompressor};
use crate::config::SKIP_BUFFER;
use crate::index::{EntryMeta, Method, VERIFY_BAD, VERIFY_OK};
use crate::source::BufSource;
use crc32fast::Hasher as Crc;
use rawzip::FileReader;
use std::io;
use std::sync::Arc;

/// The parts of a decoder that do not depend on which entry it reads.
///
/// These are what a decoder is worth recycling for: the zstd context alone is
/// several megabytes.
#[derive(Debug)]
pub struct CoreParts {
    pub codec: Codec,
    pub buf: Box<[u8]>,
    pub skip: Box<[u8]>,
}

/// Where an entry's compressed bytes are and how to read them.
#[derive(Clone, Copy, Debug)]
pub struct EntryLocation {
    pub data_start: u64,
    pub compressed_size: u64,
    pub method: Method,
}

/// A decoder together with the offset it has reached.
#[derive(Debug)]
pub struct PositionedDecoder {
    /// The next uncompressed offset this decoder will produce.
    pos: u64,
    /// The codec has reported the end of the stream.
    finished: bool,
    /// Set once the entry has been checked, so it is not checked twice.
    verified: bool,
    crc: Option<Crc>,
    codec: Codec,
    src: BufSource,
    skip: Box<[u8]>,
}

impl PositionedDecoder {
    /// Builds a decoder at offset zero from recycled parts.
    pub fn new(
        reader: Arc<FileReader>,
        loc: EntryLocation,
        parts: CoreParts,
        verify: bool,
    ) -> PositionedDecoder {
        let CoreParts { codec, buf, skip } = parts;
        let mut decoder = PositionedDecoder {
            pos: 0,
            finished: false,
            verified: false,
            crc: verify.then(Crc::new),
            codec,
            src: BufSource::new(reader, loc.data_start, loc.compressed_size, buf),
            skip,
        };
        decoder.codec.reinit();
        decoder
    }

    /// The next offset this decoder can produce without skipping.
    #[inline]
    #[must_use]
    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// Whether the stream has run out.
    #[inline]
    #[must_use]
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Whether this decoder can read an entry compressed with `method`.
    #[inline]
    #[must_use]
    pub fn handles(&self, method: Method) -> bool {
        self.codec.method() == method
    }

    /// Returns to offset zero, keeping the codec context and both buffers.
    pub fn reinit(&mut self, loc: EntryLocation) {
        self.codec.reinit();
        self.src.retarget(loc.data_start, loc.compressed_size);
        self.pos = 0;
        self.finished = false;
        self.verified = false;
        if let Some(crc) = &mut self.crc {
            crc.reset();
        }
    }

    /// Gives the expensive parts back for another decoder to use.
    #[must_use]
    pub fn into_parts(self) -> CoreParts {
        CoreParts {
            codec: self.codec,
            buf: self.src.into_buf(),
            skip: self.skip,
        }
    }

    /// Fills `out` with the entry's bytes starting at `offset`.
    ///
    /// `offset` must not be behind the decoder's position; the pool is what
    /// guarantees that, by rewinding a decoder before handing it back out.
    /// A short result means the entry ended.
    ///
    /// # Errors
    ///
    /// Returns an error when the offset is invalid, the entry is truncated, or
    /// decompression or checksum verification fails.
    ///
    pub fn read_at(&mut self, offset: u64, out: &mut [u8], meta: &EntryMeta) -> io::Result<usize> {
        if self.pos > offset {
            // The pool rewinds a decoder before handing it out, so this cannot
            // happen. Reporting it beats returning bytes from the wrong place.
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "decoder is past the requested offset",
            ));
        }
        let total = meta.uncompressed_size;
        let PositionedDecoder {
            pos,
            finished,
            crc,
            codec,
            src,
            skip,
            ..
        } = self;
        let mut pump = Pump {
            src,
            codec,
            crc,
            pos,
            finished,
        };

        while *pump.pos < offset {
            let want = usize::try_from(offset - *pump.pos)
                .unwrap_or(usize::MAX)
                .min(skip.len());
            if pump.step(&mut skip[..want])? == 0 {
                return Err(truncated());
            }
        }

        let mut written = 0;
        while written < out.len() && *pump.pos < total {
            let room = usize::try_from(total - *pump.pos)
                .unwrap_or(usize::MAX)
                .min(out.len() - written);
            let n = pump.step(&mut out[written..written + room])?;
            if n == 0 {
                break;
            }
            written += n;
        }

        if self.pos >= total {
            self.verify(meta)?;
        }
        Ok(written)
    }

    /// Checks the finished entry against the central directory.
    fn verify(&mut self, meta: &EntryMeta) -> io::Result<()> {
        if self.verified {
            return Ok(());
        }
        self.verified = true;
        if self.pos != meta.uncompressed_size {
            meta.set_verify_state(VERIFY_BAD);
            return Err(truncated());
        }
        let Some(crc) = &self.crc else {
            return Ok(());
        };
        let sum = crc.clone().finalize();

        // A stream that keeps going past its declared size is as wrong as one
        // that stops early, so look for one more byte before accepting it.
        let mut extra = [0u8; 1];
        let PositionedDecoder {
            pos,
            finished,
            crc,
            codec,
            src,
            ..
        } = self;
        let mut pump = Pump {
            src,
            codec,
            crc,
            pos,
            finished,
        };
        let overrun = match pump.step(&mut extra) {
            Ok(n) => n > 0,
            Err(e) => {
                meta.set_verify_state(VERIFY_BAD);
                return Err(e);
            }
        };

        if sum != meta.crc32 || overrun {
            meta.set_verify_state(VERIFY_BAD);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "entry does not match its checksum",
            ));
        }
        meta.set_verify_state(VERIFY_OK);
        Ok(())
    }
}

fn truncated() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "entry is shorter than the central directory says",
    )
}

/// The borrowed halves of a decoder that one decompression step needs.
///
/// This exists so a step can write into the decoder's own skip buffer without
/// borrowing the whole decoder twice.
struct Pump<'a> {
    src: &'a mut BufSource,
    codec: &'a mut Codec,
    crc: &'a mut Option<Crc>,
    pos: &'a mut u64,
    finished: &'a mut bool,
}

impl Pump<'_> {
    /// Produces at least one byte into `out`, or zero at the end of the stream.
    fn step(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || *self.finished {
            return Ok(0);
        }
        loop {
            let input = self.src.fill_buf()?;
            if input.is_empty() {
                // Stored entries never announce an end; they simply run out.
                *self.finished = true;
                return Ok(0);
            }
            let step = self.codec.step(input, out)?;
            self.src.consume(step.consumed);
            if step.produced > 0 {
                if let Some(crc) = self.crc.as_mut() {
                    crc.update(&out[..step.produced]);
                }
                *self.pos += step.produced as u64;
                if step.done {
                    *self.finished = true;
                }
                return Ok(step.produced);
            }
            if step.done {
                *self.finished = true;
                return Ok(0);
            }
            if step.consumed == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "decompressor stalled",
                ));
            }
        }
    }
}

use std::io::BufRead as _;

/// The default size of a decoder's skip buffer.
pub const DEFAULT_SKIP: usize = SKIP_BUFFER;
