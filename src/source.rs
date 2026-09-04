//! A buffered reader over one byte range of the archive.

use rawzip::{FileReader, ReaderAt};
use std::io::{self, BufRead, Read};
use std::sync::Arc;

/// Reads one entry's compressed bytes in large positional chunks.
///
/// Every refill is a single `pread` of up to the buffer size, so an archive on
/// a network filesystem sees a few large sequential requests instead of a
/// stream of small ones. The buffer is never resized, so a decoder that is
/// recycled onto another entry keeps it.
#[derive(Debug)]
pub struct BufSource {
    reader: Arc<FileReader>,
    start: u64,
    len: u64,
    /// Bytes of the range that have already left the buffer.
    consumed: u64,
    buf: Box<[u8]>,
    filled: usize,
    pos: usize,
}

impl BufSource {
    /// Wraps `[start, start + len)` of the file.
    #[must_use]
    pub fn new(reader: Arc<FileReader>, start: u64, len: u64, buf: Box<[u8]>) -> BufSource {
        BufSource {
            reader,
            start,
            len,
            consumed: 0,
            buf,
            filled: 0,
            pos: 0,
        }
    }

    /// Points the reader at a different range and starts over.
    pub fn retarget(&mut self, start: u64, len: u64) {
        self.start = start;
        self.len = len;
        self.rewind();
    }

    /// Goes back to the start of the range.
    pub fn rewind(&mut self) {
        self.consumed = 0;
        self.filled = 0;
        self.pos = 0;
    }

    /// Bytes of the range that have not been handed out yet.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.len - self.consumed - self.pos as u64
    }

    /// Gives the buffer back so another decoder can use it.
    #[must_use]
    pub fn into_buf(self) -> Box<[u8]> {
        self.buf
    }
}

impl BufRead for BufSource {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos == self.filled {
            self.consumed += self.filled as u64;
            self.pos = 0;
            self.filled = 0;
            let remaining = self.len - self.consumed;
            if remaining == 0 {
                return Ok(&[]);
            }
            let want = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(self.buf.len());
            self.reader
                .read_exact_at(&mut self.buf[..want], self.start + self.consumed)?;
            self.filled = want;
        }
        Ok(&self.buf[self.pos..self.filled])
    }

    fn consume(&mut self, amount: usize) {
        self.pos = (self.pos + amount).min(self.filled);
    }
}

impl Read for BufSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}
