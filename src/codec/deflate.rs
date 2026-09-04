//! Raw deflate, as zip stores it.

use super::{CodecError, Decompressor, Step};
use flate2::{Decompress, FlushDecompress, Status};

#[derive(Debug)]
pub struct Deflate {
    inner: Decompress,
}

impl Deflate {
    pub fn new() -> Deflate {
        // `false` means no zlib header. Zip entries hold a bare deflate stream.
        Deflate {
            inner: Decompress::new(false),
        }
    }
}

impl Default for Deflate {
    fn default() -> Self {
        Deflate::new()
    }
}

impl Decompressor for Deflate {
    fn reinit(&mut self) {
        self.inner.reset(false);
    }

    fn step(&mut self, input: &[u8], output: &mut [u8]) -> Result<Step, CodecError> {
        let before_in = self.inner.total_in();
        let before_out = self.inner.total_out();
        let status = self
            .inner
            .decompress(input, output, FlushDecompress::None)
            .map_err(|e| CodecError(format!("deflate: {e}")))?;
        let consumed = usize::try_from(self.inner.total_in() - before_in)
            .map_err(|_error| CodecError("deflate input count overflow".into()))?;
        let produced = usize::try_from(self.inner.total_out() - before_out)
            .map_err(|_error| CodecError("deflate output count overflow".into()))?;
        Ok(Step {
            consumed,
            produced,
            done: status == Status::StreamEnd,
        })
    }
}
