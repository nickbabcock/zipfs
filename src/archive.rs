//! The archive file and the one piece of state that is filled in lazily.

use crate::guard::{Change, Freshness, GuardedFile};
use crate::index::EntryMeta;
use log::debug;
use rawzip::{ReaderAt, ZipArchive, ZipLocator};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The length of the end of central directory record without its comment
/// (APPNOTE.TXT 4.3.16).
const EOCD_FIXED: u64 = 22;

/// The last bytes of the central directory, up to the end of the fixed part
/// of the end of central directory record.
///
/// That record holds the offset, size and entry count of the central
/// directory. Before it are the zip64 records, if there are any, and the end
/// of the central directory, with the names, checksums and offsets of the last
/// entries. So a different archive almost always differs here.
///
/// The comment comes after the record and can be up to 65535 bytes long. It is
/// not part of this, because two versions of one archive often have the same
/// comment.
#[derive(Debug)]
struct DirectoryEnd {
    at: u64,
    bytes: Box<[u8]>,
}

impl DirectoryEnd {
    /// How many bytes are kept.
    const LEN: u64 = 4096;

    fn read<R: ReaderAt>(reader: &R, eocd_offset: u64) -> io::Result<DirectoryEnd> {
        let end = eocd_offset + EOCD_FIXED;
        let at = end.saturating_sub(Self::LEN);
        let len = usize::try_from(end - at).expect("at most LEN bytes");
        let mut bytes = vec![0u8; len];
        reader.read_exact_at(&mut bytes, at)?;
        Ok(DirectoryEnd {
            at,
            bytes: bytes.into_boxed_slice(),
        })
    }

    /// Whether `reader` gives back the same bytes.
    fn matches<R: ReaderAt>(&self, reader: &R) -> io::Result<bool> {
        let mut bytes = vec![0u8; self.bytes.len()];
        match reader.read_exact_at(&mut bytes, self.at) {
            Ok(()) => Ok(bytes == *self.bytes),
            // The file is shorter than it was.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// An open zip archive.
///
/// The reader sits behind an `Arc` so that a decoder can own a handle to it.
/// A decoder lives in the handle table inside the filesystem, so it cannot
/// borrow one of the filesystem's own fields. [`ReaderAt::read_at`] takes
/// `&self` and issues a real `pread`, so concurrent readers need no
/// synchronisation at all.
///
/// Every read, the central directory included, goes through a
/// [`GuardedFile`], so no byte from a changed archive is used.
#[derive(Debug)]
pub struct Archive {
    zip: ZipArchive<Arc<GuardedFile>>,
    reader: Arc<GuardedFile>,
    /// The end of the central directory at mount time.
    directory_end: DirectoryEnd,
}

impl Archive {
    /// Opens an archive and locates its central directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or read, or when the zip
    /// archive has no valid central directory.
    pub fn open(path: &Path) -> crate::Result<Archive> {
        let reader = Arc::new(GuardedFile::open(path)?);
        let mut buf = vec![0u8; rawzip::RECOMMENDED_BUFFER_SIZE];
        let zip = ZipLocator::new()
            .locate_in_reader(Arc::clone(&reader), &mut buf, reader.len())
            .map_err(|(_, e)| crate::Error::Zip(e))?;
        // rawzip's `Arc` wrapper does not forward `read_exact_at`, so call it on
        // the `GuardedFile`. This is also true for `read_at` below.
        let directory_end = DirectoryEnd::read(reader.as_ref(), zip.eocd_offset())?;
        Ok(Archive {
            zip,
            reader,
            directory_end,
        })
    }

    /// Examines the archive fully.
    ///
    /// This is for a read whose bytes failed their checks. Besides the stamps,
    /// it compares the end of the central directory through the descriptor.
    /// That finds a descriptor that reads another file while the stamps say
    /// that nothing changed. A CIFS client causes this when it opens the path
    /// again after a reconnect.
    #[must_use]
    pub fn check(&self) -> Freshness {
        match self.reader.check() {
            Freshness::Current => {}
            found => return found,
        }
        // The stamps were compared just now, so do not compare them again.
        match self.directory_end.matches(&self.reader.unverified()) {
            Ok(true) => Freshness::Current,
            Ok(false) => {
                self.reader.mark(Change::Rewritten);
                Freshness::Stale
            }
            Err(e) => {
                debug!("could not check the archive: {e}");
                Freshness::Unknown
            }
        }
    }

    /// Returns the parsed zip archive.
    #[must_use]
    pub fn zip(&self) -> &ZipArchive<Arc<GuardedFile>> {
        &self.zip
    }

    /// A handle a decoder can own for the life of the mount.
    #[must_use]
    pub fn reader(&self) -> Arc<GuardedFile> {
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
        self.reader.as_ref().read_exact_at(buf, offset)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rawzip::{CompressionMethod, FileReader, ZipArchiveWriter};
    use std::io::Write;

    /// A file that is removed when it goes out of scope.
    struct TempFile(std::path::PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// An archive with one stored entry and a comment of 8 KiB.
    fn commented(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut writer = ZipArchiveWriter::new(&mut out);
        let (mut file, config) = writer
            .new_file("data")
            .compression_method(CompressionMethod::STORE)
            .start()
            .unwrap();
        let mut data = config.wrap(&mut file);
        data.write_all(body).unwrap();
        let (_, descriptor) = data.finish().unwrap();
        file.finish(descriptor).unwrap();
        writer.finish().unwrap();
        // The record ends with the comment length, and the comment follows.
        let len = out.len();
        out[len - 2..].copy_from_slice(&8192u16.to_le_bytes());
        out.resize(len + 8192, b'#');
        out
    }

    #[test]
    fn another_archive_with_the_same_long_comment_does_not_match() {
        let old = commented(b"first");
        let new = commented(b"second");
        // The last 4 KiB are all comment, so they cannot tell the two apart.
        assert_eq!(old[old.len() - 4096..], new[new.len() - 4096..]);

        let temp = TempFile(
            std::env::temp_dir().join(format!("zipfs-directory-end-{}.zip", std::process::id())),
        );
        let path = &temp.0;
        std::fs::write(path, &old).unwrap();
        let mut buf = vec![0u8; rawzip::RECOMMENDED_BUFFER_SIZE];
        let zip = ZipArchive::from_file(std::fs::File::open(path).unwrap(), &mut buf).unwrap();
        let directory_end = DirectoryEnd::read(zip.get_ref(), zip.eocd_offset()).unwrap();
        assert!(directory_end.matches(zip.get_ref()).unwrap());

        // This is what a CIFS client gives after it opens the path again.
        std::fs::write(path, &new).unwrap();
        let reopened = FileReader::from(std::fs::File::open(path).unwrap());
        assert!(!directory_end.matches(&reopened).unwrap());
    }
}
