//! Builds archives for the tests to read.

#![allow(
    dead_code,
    reason = "These helpers are shared by several integration tests."
)]

use rawzip::{CompressionMethod, ZipArchiveWriter};
use std::io::Write;

/// One entry to put in a fixture archive.
#[derive(Debug)]
pub struct Entry {
    pub path: &'static str,
    pub body: Vec<u8>,
    pub method: CompressionMethod,
    pub mode: Option<u32>,
}

impl Entry {
    #[must_use]
    pub fn file(path: &'static str, body: impl Into<Vec<u8>>, method: CompressionMethod) -> Entry {
        Entry {
            path,
            body: body.into(),
            method,
            mode: None,
        }
    }

    #[must_use]
    pub fn symlink(path: &'static str, target: &str) -> Entry {
        Entry {
            path,
            body: target.as_bytes().to_vec(),
            method: CompressionMethod::STORE,
            mode: Some(0o120_777),
        }
    }

    #[must_use]
    pub fn mode(mut self, mode: u32) -> Entry {
        self.mode = Some(mode);
        self
    }
}

/// Writes an archive holding the given entries.
#[must_use]
///
/// # Panics
///
/// Panics when the fixture archive cannot be written.
pub fn archive(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = ZipArchiveWriter::new(&mut out);
    for entry in entries {
        if entry.path.ends_with('/') {
            writer.new_dir(entry.path).create().unwrap();
            continue;
        }
        let mut builder = writer.new_file(entry.path).compression_method(entry.method);
        if let Some(mode) = entry.mode {
            builder = builder.unix_permissions(mode);
        }
        let (mut file, config) = builder.start().unwrap();
        match entry.method {
            CompressionMethod::STORE => {
                let mut data = config.wrap(&mut file);
                data.write_all(&entry.body).unwrap();
                let (_, descriptor) = data.finish().unwrap();
                file.finish(descriptor).unwrap();
            }
            CompressionMethod::DEFLATE => {
                let encoder =
                    flate2::write::DeflateEncoder::new(&mut file, flate2::Compression::default());
                let mut data = config.wrap(encoder);
                data.write_all(&entry.body).unwrap();
                let (encoder, descriptor) = data.finish().unwrap();
                encoder.finish().unwrap();
                file.finish(descriptor).unwrap();
            }
            CompressionMethod::ZSTD => {
                let encoder = zstd::Encoder::new(&mut file, 3).unwrap();
                let mut data = config.wrap(encoder);
                data.write_all(&entry.body).unwrap();
                let (encoder, descriptor) = data.finish().unwrap();
                encoder.finish().unwrap();
                file.finish(descriptor).unwrap();
            }
            CompressionMethod::DEFLATE64 => {
                // The wrapper only records the size and the checksum of the body.
                let mut data = config.wrap(std::io::sink());
                data.write_all(&entry.body).unwrap();
                let (_, descriptor) = data.finish().unwrap();
                file.write_all(&stored_blocks(&entry.body)).unwrap();
                file.finish(descriptor).unwrap();
            }
            other => panic!("the fixture writer cannot produce method {other:?}"),
        }
    }
    writer.finish().unwrap();
    out
}

/// Compresses `body` to a raw deflate stream.
///
/// # Panics
///
/// Panics when the encoder fails, which cannot occur for a memory buffer.
#[must_use]
pub fn deflate(body: &[u8]) -> Vec<u8> {
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(body).unwrap();
    encoder.finish().unwrap()
}

/// Encodes `body` as stored blocks, which deflate and Deflate64 read the same.
///
/// No encoder for Deflate64 is available to the tests. This gives a valid stream
/// for any body, but it does not use the features that only Deflate64 has.
///
/// # Panics
///
/// Panics when a block is longer than a stored block can be, which cannot occur.
#[must_use]
pub fn stored_blocks(body: &[u8]) -> Vec<u8> {
    let mut chunks = body.chunks(usize::from(u16::MAX)).peekable();
    let mut out = Vec::with_capacity(body.len() + 5);
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        let len = u16::try_from(chunk.len()).unwrap();
        out.push(u8::from(chunks.peek().is_none()));
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out
}

/// Writes an archive with one entry that holds a stream made by hand.
///
/// # Panics
///
/// Panics when the fixture archive cannot be written.
#[must_use]
pub fn archive_with_stream(
    path: &str,
    method: CompressionMethod,
    stream: &[u8],
    body: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = ZipArchiveWriter::new(&mut out);
    let (mut file, config) = writer
        .new_file(path)
        .compression_method(method)
        .start()
        .unwrap();
    // The wrapper only records the size and the checksum of `body`.
    let mut data = config.wrap(std::io::sink());
    data.write_all(body).unwrap();
    let (_, descriptor) = data.finish().unwrap();
    file.write_all(stream).unwrap();
    file.finish(descriptor).unwrap();
    writer.finish().unwrap();
    out
}

/// An archive written to a temporary file, removed when it goes out of scope.
#[derive(Debug)]
pub struct TempArchive {
    pub path: std::path::PathBuf,
}

impl TempArchive {
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics when the temporary archive cannot be written.
    pub fn new(name: &str, bytes: &[u8]) -> TempArchive {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "zipfs-test-{}-{}-{name}.zip",
            std::process::id(),
            next_id()
        ));
        std::fs::write(&path, bytes).unwrap();
        TempArchive { path }
    }
}

impl Drop for TempArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Repeatable pseudo-random bytes, so a failure can be reproduced.
#[must_use]
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Text that compresses well, so deflate and zstd have something to do.
#[must_use]
pub fn compressible(len: usize) -> Vec<u8> {
    let phrase = b"the quick brown fox jumps over the lazy dog. ";
    phrase.iter().copied().cycle().take(len).collect()
}
