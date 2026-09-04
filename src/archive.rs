//! The archive file and the one piece of state that is filled in lazily.

use crate::index::EntryMeta;
use rawzip::{FileReader, ReaderAt, ZipArchive, ZipLocator};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// An open zip archive.
///
/// The reader sits behind an `Arc` so that a decoder can own a handle to it.
/// A decoder lives in the handle table inside the filesystem, so it cannot
/// borrow one of the filesystem's own fields. [`ReaderAt::read_at`] takes
/// `&self` and issues a real `pread`, so concurrent readers need no
/// synchronisation at all.
#[derive(Debug)]
pub struct Archive {
    zip: ZipArchive<Arc<FileReader>>,
    reader: Arc<FileReader>,
    /// The archive's length and modification time when it was opened, so a
    /// change underneath the mount can be reported.
    pub len: u64,
    pub mtime: std::time::SystemTime,
}

impl Archive {
    /// Opens an archive and locates its central directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or read, or when the zip
    /// archive has no valid central directory.
    pub fn open(path: &Path) -> crate::Result<Archive> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        let len = meta.len();
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let reader = Arc::new(FileReader::from(file));
        let mut buf = vec![0u8; rawzip::RECOMMENDED_BUFFER_SIZE];
        let zip = ZipLocator::new()
            .locate_in_reader(Arc::clone(&reader), &mut buf, len)
            .map_err(|(_, e)| crate::Error::Zip(e))?;
        Ok(Archive {
            zip,
            reader,
            len,
            mtime,
        })
    }

    /// Returns the parsed zip archive.
    #[must_use]
    pub fn zip(&self) -> &ZipArchive<Arc<FileReader>> {
        &self.zip
    }

    /// A handle a decoder can own for the life of the mount.
    #[must_use]
    pub fn reader(&self) -> Arc<FileReader> {
        Arc::clone(&self.reader)
    }

    /// How many entries the archive claims to hold.
    ///
    /// This comes from the end of central directory record and is not checked
    /// against anything, so treat it as a hint only.
    #[must_use]
    pub fn entries_hint(&self) -> u64 {
        self.zip.entries_hint()
    }

    /// Reads bytes straight out of the file.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested bytes cannot be read from the file.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.reader.read_exact_at(buf, offset)
    }

    /// Where an entry's compressed bytes begin.
    ///
    /// Finding this means reading the entry's local header, which is one small
    /// read. The answer is cached on the entry, so an archive on a remote
    /// filesystem pays it once rather than once per open.
    ///
    /// Two threads may resolve the same entry at once. That is harmless: the
    /// answer does not depend on who asks, so the loser simply repeats a read
    /// that has already been done.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry's local header cannot be read or does
    /// not match the central directory.
    pub fn data_start(&self, meta: &EntryMeta) -> crate::Result<u64> {
        let cached = meta.data_start.load(Ordering::Relaxed);
        if cached != u64::MAX {
            return Ok(cached);
        }
        let entry = self.zip.get_entry(meta.wayfinder)?;
        let (start, end) = entry.compressed_data_range();
        let Some(length) = end.checked_sub(start) else {
            return Err(crate::Error::Corrupt(
                "local header has an invalid compressed data range",
            ));
        };
        if length != meta.compressed_size {
            return Err(crate::Error::Corrupt(
                "local header disagrees with the central directory",
            ));
        }
        meta.data_start.store(start, Ordering::Relaxed);
        Ok(start)
    }
}
