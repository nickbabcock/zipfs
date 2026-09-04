//! Decompressors, driven a buffer at a time.
//!
//! These wrap the raw codec interfaces rather than the `Read` adapters, for
//! three reasons: a decoder can be reset onto a new stream without discarding
//! its context, all methods share one skip and checksum loop, and output goes
//! straight into the reply buffer with no copy in between.

mod deflate;
#[cfg(feature = "zstd")]
mod zstd;

use crate::index::Method;
use std::fmt;

/// What one call to [`Decompressor::step`] achieved.
#[derive(Debug, Clone, Copy, Default)]
pub struct Step {
    /// Compressed bytes taken from the input.
    pub consumed: usize,
    /// Uncompressed bytes written to the output.
    pub produced: usize,
    /// The stream ended.
    pub done: bool,
}

/// A decompressor could not make sense of its input.
#[derive(Debug)]
pub struct CodecError(pub String);

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CodecError {}

impl From<CodecError> for std::io::Error {
    fn from(e: CodecError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.0)
    }
}

/// Turns compressed bytes into uncompressed ones.
pub trait Decompressor: Send {
    /// Discards all stream state so the decoder can read another entry from
    /// its start. Any expensive context is kept.
    fn reinit(&mut self);

    /// Moves as much of `input` into `output` as it can.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is not a valid stream for the codec.
    fn step(&mut self, input: &[u8], output: &mut [u8]) -> Result<Step, CodecError>;
}

/// A decompressor for one of the supported methods.
#[derive(Debug)]
pub enum Codec {
    /// Stored entries are copied through unchanged.
    Store(Store),
    Deflate(deflate::Deflate),
    #[cfg(feature = "zstd")]
    Zstd(zstd::Zstd),
}

impl Codec {
    #[must_use]
    pub fn new(method: Method) -> Codec {
        match method {
            Method::Store => Codec::Store(Store),
            Method::Deflate => Codec::Deflate(deflate::Deflate::new()),
            #[cfg(feature = "zstd")]
            Method::Zstd => Codec::Zstd(zstd::Zstd::new()),
        }
    }

    /// Which method this decoder currently reads.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Codec::Store(_) => Method::Store,
            Codec::Deflate(_) => Method::Deflate,
            #[cfg(feature = "zstd")]
            Codec::Zstd(_) => Method::Zstd,
        }
    }

    fn inner_mut(&mut self) -> &mut dyn Decompressor {
        match self {
            Codec::Store(c) => c,
            Codec::Deflate(c) => c,
            #[cfg(feature = "zstd")]
            Codec::Zstd(c) => c,
        }
    }
}

impl Decompressor for Codec {
    fn reinit(&mut self) {
        self.inner_mut().reinit();
    }

    fn step(&mut self, input: &[u8], output: &mut [u8]) -> Result<Step, CodecError> {
        self.inner_mut().step(input, output)
    }
}

/// The identity decompressor used for stored entries.
#[derive(Debug)]
pub struct Store;

impl Decompressor for Store {
    fn reinit(&mut self) {}

    fn step(&mut self, input: &[u8], output: &mut [u8]) -> Result<Step, CodecError> {
        let n = input.len().min(output.len());
        output[..n].copy_from_slice(&input[..n]);
        Ok(Step {
            consumed: n,
            produced: n,
            // A stored entry ends when its range does, which the caller knows.
            done: false,
        })
    }
}
