//! Deflate64, as zip stores it.

use super::{CodecError, Decompressor, Step};
use deflate64::InflaterManaged;

#[derive(Debug)]
pub struct Deflate64 {
    // The inflater keeps its tables inline, so it is too large to move by value.
    inner: Box<InflaterManaged>,
}

impl Deflate64 {
    pub fn new() -> Deflate64 {
        Deflate64 {
            inner: Box::new(InflaterManaged::new()),
        }
    }
}

impl Default for Deflate64 {
    fn default() -> Self {
        Deflate64::new()
    }
}

impl Decompressor for Deflate64 {
    fn reinit(&mut self) {
        *self.inner = InflaterManaged::new();
    }

    fn step(&mut self, input: &[u8], output: &mut [u8]) -> Result<Step, CodecError> {
        let result = self.inner.inflate(input, output);
        if result.data_error {
            return Err(CodecError("deflate64: invalid stream".into()));
        }
        Ok(Step {
            consumed: result.bytes_consumed,
            produced: result.bytes_written,
            // This stays false until the inflater has delivered all of its output.
            done: self.inner.finished(),
        })
    }
}
