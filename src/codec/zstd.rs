//! Zstandard, as stored by methods 93 and 20.

use super::{CodecError, Decompressor, Step};
use std::fmt;
use zstd::stream::raw::{Decoder, Operation};

pub struct Zstd {
    inner: Decoder<'static>,
}

impl fmt::Debug for Zstd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Zstd").finish_non_exhaustive()
    }
}

impl Zstd {
    pub fn new() -> Zstd {
        Zstd {
            // Allocating the context is the expensive part, which is why a
            // decoder is reset rather than replaced when it changes entries.
            inner: Decoder::new().expect("zstd context"),
        }
    }
}

impl Default for Zstd {
    fn default() -> Self {
        Zstd::new()
    }
}

impl Decompressor for Zstd {
    fn reinit(&mut self) {
        let _ = self.inner.reinit();
    }

    fn step(&mut self, input: &[u8], output: &mut [u8]) -> Result<Step, CodecError> {
        let status = self
            .inner
            .run_on_buffers(input, output)
            .map_err(|e| CodecError(format!("zstd: {e}")))?;
        Ok(Step {
            consumed: status.bytes_read,
            produced: status.bytes_written,
            // zstd reports no hint for further input once a frame is complete,
            // and a zip entry holds exactly one frame.
            done: status.remaining == 0,
        })
    }
}
