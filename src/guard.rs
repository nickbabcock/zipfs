//! The archive file, read so that bytes from a changed archive are never
//! returned.
//!
//! The index holds offsets into the archive as it was at mount time. If the
//! file is written over, those offsets point at other bytes. A compressed
//! entry then usually fails to decode. But a stored entry has no check that
//! catches it, and the kernel keeps whatever was returned in its page cache.
//!
//! So every read is verified after it completes and before its bytes are used.
//! The kernel serves repeated reads from its page cache, so only reads that
//! reach the archive pay for this.
//!
//! On a local filesystem, an open descriptor keeps the file that was opened.
//! A rename, a removal or a new file at the path does not change what the
//! descriptor reads. So only the open file is examined, with one `fstat`. The
//! kernel updates the modification time before `write(2)` copies data in. So
//! if the size and the time are the same after a read as at mount time,
//! usually no write had started before the read ended.
//!
//! This check does not find a write that keeps the size and the recorded
//! modification time. The time can stay the same when the filesystem records
//! time in large steps, and a write comes in the same step as the last write
//! before the mount. A tool can also set the old time again after it writes.
//! A write through a shared mapping updates the time only when it makes a
//! clean page dirty, so later writes to that page can go unnoticed.
//!
//! On NFS, the size and time come from the attribute cache, so a change made
//! on another host is found only once the cache expires.
//!
//! The change time is not compared. It also moves when only the metadata
//! changes, for example after `chmod` or a new hard link. The contents are
//! then the same.
//!
//! A CIFS client opens the path again after it reconnects. From then on, the
//! old descriptor reads the file that has that name now. So on CIFS the path
//! is examined too, after each read. The name is found from a descriptor of
//! the directory that was opened before the mount. So a mount on top of that
//! directory does not hide the archive. The times come from the client's
//! attribute cache, which the `actimeo` mount option refreshes. A replacement
//! is usually found within that time, before a reconnect can move the
//! descriptor to the new file. Until the cache is refreshed, a descriptor that
//! has already moved is not found, so on CIFS this protection is not
//! guaranteed.

use log::{error, info, warn};
use rawzip::ReaderAt;
use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// `CIFS_SUPER_MAGIC` and `SMB2_SUPER_MAGIC` from `<linux/magic.h>`.
const CIFS_MAGIC: [u32; 2] = [0xFF53_4D42, 0xFE53_4D42];

/// What identifies one version of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
}

impl Stamp {
    fn of(meta: &Metadata) -> Stamp {
        Stamp {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
        }
    }

    #[allow(
        clippy::cast_sign_loss,
        clippy::useless_conversion,
        reason = "The types of these fields are different on each target."
    )]
    fn of_stat(st: &libc::stat) -> Stamp {
        Stamp {
            dev: u64::from(st.st_dev),
            ino: u64::from(st.st_ino),
            len: st.st_size as u64,
            mtime: (i64::from(st.st_mtime), i64::from(st.st_mtime_nsec)),
        }
    }

    fn same_file(&self, other: &Stamp) -> bool {
        (self.dev, self.ino) == (other.dev, other.ino)
    }

    /// Whether nothing was written since the other stamp.
    fn unchanged(&self, other: &Stamp) -> bool {
        (self.len, self.mtime) == (other.len, other.mtime)
    }
}

/// Why the archive is no longer the one the index describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// Nothing is at the path now.
    Removed,
    /// The path names a different file.
    Replaced,
    /// The file has a different size or modification time.
    Modified,
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Change::Removed => "was removed",
            Change::Replaced => "was replaced by another file",
            Change::Modified => "was modified",
        })
    }
}

/// What a check of the archive found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The archive is the one that was mounted.
    Current,
    /// The archive changed after it was mounted. This is permanent.
    Stale,
    /// The check could not finish, for example because the network is down.
    Unknown,
}

fn stale() -> io::Error {
    io::Error::from_raw_os_error(libc::ESTALE)
}

/// Whether the descriptor can start to read another file that takes its path.
fn descriptor_follows_path(file: &File) -> io::Result<bool> {
    let mut st = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the descriptor is open for the life of `file`, and `st` is large
    // enough for the result.
    if unsafe { libc::fstatfs(file.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstatfs` succeeded, so it filled in `st`.
    let st = unsafe { st.assume_init() };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "The type of f_type is different on each target. The magic \
                  numbers are 32 bits."
    )]
    let kind = st.f_type as u32;
    Ok(CIFS_MAGIC.contains(&kind))
}

/// The archive's name in the directory that held it at mount time.
#[derive(Debug)]
struct Named {
    /// The directory, opened with `O_PATH`. A lookup from it is not affected by
    /// a mount on top of the directory, or by a rename of the directory.
    dir: File,
    name: CString,
}

impl Named {
    /// Finds the directory and the name with all links resolved. A link such
    /// as `/dev/stdin` names another file after the daemon detaches.
    fn open(path: &Path) -> io::Result<Named> {
        let path = std::fs::canonicalize(path)?;
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        };
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(dir)?;
        Ok(Named {
            dir,
            name: CString::new(OsStr::as_bytes(name))?,
        })
    }

    fn stamp(&self) -> io::Result<Stamp> {
        let mut st = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the descriptor is open for the life of `self.dir`, the name
        // ends with a NUL, and `st` is large enough for the result.
        let rc =
            unsafe { libc::fstatat(self.dir.as_raw_fd(), self.name.as_ptr(), st.as_mut_ptr(), 0) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fstatat` succeeded, so it filled in `st`.
        Ok(Stamp::of_stat(unsafe { st.assume_init_ref() }))
    }
}

/// The open archive, and what it was when it was opened.
#[derive(Debug)]
pub struct GuardedFile {
    file: File,
    /// The path as it was given, for messages only.
    path: PathBuf,
    /// Where to look for the archive again. This is only set where the
    /// descriptor can follow the path to another file.
    named: Option<Named>,
    /// The open file's stamp at mount time. The path is compared with this
    /// too, so a file that took the name before the stamp was made is a
    /// change as well.
    opened: Stamp,
    stale: AtomicBool,
    /// The last check could not finish. This is not permanent.
    unsure: AtomicBool,
}

impl GuardedFile {
    /// Opens the archive and records what it is.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or examined.
    pub fn open(path: &Path) -> io::Result<GuardedFile> {
        let file = File::open(path)?;
        let follows = descriptor_follows_path(&file).unwrap_or_else(|e| {
            warn!(
                "cannot find the filesystem of archive '{}': {e}; \
                 a replacement at that path will not be found",
                path.display()
            );
            false
        });
        GuardedFile::new(file, path, follows)
    }

    fn new(file: File, path: &Path, check_path: bool) -> io::Result<GuardedFile> {
        let opened = Stamp::of(&file.metadata()?);
        let named = if check_path {
            // A memfd or a removed file has no name to find again. It also
            // cannot be replaced, so the open file is enough.
            Named::open(path)
                .inspect_err(|e| {
                    warn!(
                        "cannot find the directory of archive '{}': {e}; \
                         a replacement at that path will not be found",
                        path.display()
                    );
                })
                .ok()
        } else {
            None
        };
        Ok(GuardedFile {
            file,
            path: path.to_path_buf(),
            named,
            opened,
            stale: AtomicBool::new(false),
            unsure: AtomicBool::new(false),
        })
    }

    /// The archive's length when it was opened.
    #[must_use]
    pub(crate) fn len(&self) -> u64 {
        self.opened.len
    }

    /// Whether a change was found before. This costs one atomic load.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.stale.load(Ordering::Relaxed)
    }

    /// Compares the stamps again.
    #[must_use]
    pub(crate) fn check(&self) -> Freshness {
        match self.verify() {
            Ok(()) => Freshness::Current,
            Err(_) if self.is_stale() => Freshness::Stale,
            Err(_) => Freshness::Unknown,
        }
    }

    /// Fails unless the archive is still the one that was opened.
    ///
    /// When the answer is not known, for example because the network is down,
    /// this fails too, because bytes that cannot be verified are not used.
    /// That failure is not permanent.
    fn verify(&self) -> io::Result<()> {
        if self.is_stale() {
            return Err(stale());
        }
        match self.compare_stamps() {
            Ok(None) => {
                if self.unsure.load(Ordering::Relaxed) && self.unsure.swap(false, Ordering::Relaxed)
                {
                    info!("archive '{}' can be examined again", self.path.display());
                }
                Ok(())
            }
            Ok(Some(change)) => {
                self.mark(change);
                Err(stale())
            }
            Err(e) => {
                if !self.unsure.swap(true, Ordering::Relaxed) {
                    warn!(
                        "cannot examine archive '{}': {e}; reads fail with EIO \
                         until it can be examined again",
                        self.path.display()
                    );
                }
                Err(io::Error::other(format!("cannot examine the archive: {e}")))
            }
        }
    }

    /// Records a change. It is permanent, and it is logged one time.
    fn mark(&self, change: Change) {
        if !self.stale.swap(true, Ordering::Relaxed) {
            error!(
                "archive '{}' {change} since it was mounted; reads fail with ESTALE \
                 until it is mounted again",
                self.path.display()
            );
        }
    }

    fn compare_stamps(&self) -> io::Result<Option<Change>> {
        if !Stamp::of(&self.file.metadata()?).unchanged(&self.opened) {
            return Ok(Some(Change::Modified));
        }
        let Some(named) = &self.named else {
            return Ok(None);
        };
        let named = match named.stamp() {
            Ok(stamp) => stamp,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(Change::Removed)),
            Err(e) => return Err(e),
        };
        if !named.same_file(&self.opened) {
            return Ok(Some(Change::Replaced));
        }
        if !named.unchanged(&self.opened) {
            return Ok(Some(Change::Modified));
        }
        Ok(None)
    }
}

impl ReaderAt for GuardedFile {
    /// Reads and then verifies. A read that failed is verified too, because a
    /// change can cause the failure, for example when the file is shorter now.
    /// The change is then reported instead.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let read = self.file.read_at(buf, offset);
        self.verify()?;
        read
    }

    /// Reads all of `buf` and then verifies it once.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let read = self.file.read_exact_at(buf, offset);
        self.verify()?;
        read
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A directory that holds one file, `a.zip`. The contents do not matter
    /// here, because only the stamps are compared.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let dir =
                std::env::temp_dir().join(format!("zipfs-guard-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("a.zip"), b"first").unwrap();
            Dir(dir)
        }

        fn archive(&self) -> PathBuf {
            self.0.join("a.zip")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Opens the archive as on CIFS, where the path is examined too.
    fn open_checking_path(path: &Path) -> GuardedFile {
        GuardedFile::new(File::open(path).unwrap(), path, true).unwrap()
    }

    #[test]
    fn an_archive_replaced_by_rename_is_stale() {
        let dir = Dir::new("rename");
        let guarded = open_checking_path(&dir.archive());
        let next = dir.0.join("next.zip");
        std::fs::write(&next, b"first").unwrap();
        std::fs::rename(&next, dir.archive()).unwrap();
        // The bytes are the same, but the path names another file, which a
        // CIFS client can start to read at any time.
        assert_eq!(guarded.check(), Freshness::Stale);
    }

    #[test]
    fn a_removed_archive_is_stale() {
        let dir = Dir::new("removed");
        let guarded = open_checking_path(&dir.archive());
        std::fs::remove_file(dir.archive()).unwrap();
        assert_eq!(guarded.check(), Freshness::Stale);
    }

    #[test]
    fn a_renamed_directory_is_not_a_change() {
        let dir = Dir::new("dir-renamed");
        let guarded = open_checking_path(&dir.archive());
        let moved = Dir(dir.0.with_extension("moved"));
        std::fs::rename(&dir.0, &moved.0).unwrap();
        assert_eq!(guarded.check(), Freshness::Current);
    }

    #[test]
    fn the_path_is_followed_to_the_archive_when_it_is_opened() {
        let dir = Dir::new("symlink");
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(dir.archive(), &link).unwrap();
        let guarded = open_checking_path(&link);
        // The link can point somewhere else, as `/dev/stdin` does after the
        // daemon detaches, while the archive stays the same.
        std::fs::remove_file(&link).unwrap();
        assert_eq!(guarded.check(), Freshness::Current);
    }

    #[test]
    fn an_archive_without_a_name_opens() {
        let dir = Dir::new("unnamed");
        let file = File::open(dir.archive()).unwrap();
        std::fs::remove_file(dir.archive()).unwrap();
        // The link names "<path> (deleted)", which does not exist.
        let link = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let guarded = open_checking_path(&link);
        assert!(guarded.named.is_none());
        assert_eq!(guarded.check(), Freshness::Current);
    }

    #[test]
    fn a_path_that_cannot_be_examined_is_not_a_change() {
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: root can search any directory");
            return;
        }
        let dir = Dir::new("perm");
        let guarded = open_checking_path(&dir.archive());
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(guarded.check(), Freshness::Unknown);
        let mut buf = [0u8; 5];
        assert!(guarded.read_exact_at(&mut buf, 0).is_err());
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(guarded.check(), Freshness::Current);
        guarded.read_exact_at(&mut buf, 0).unwrap();
        assert!(!guarded.is_stale());
    }

    #[test]
    fn a_mount_on_top_of_the_directory_does_not_hide_the_archive() {
        let fusermount = [
            "/usr/bin/fusermount3",
            "/bin/fusermount3",
            "/usr/bin/fusermount",
        ];
        if !Path::new("/dev/fuse").exists() || !fusermount.iter().any(|p| Path::new(p).exists()) {
            assert!(
                std::env::var_os("ZIPFS_REQUIRE_FUSE").is_none(),
                "FUSE is required but /dev/fuse or fusermount3 is not available"
            );
            eprintln!("skipping: /dev/fuse or fusermount3 is not available");
            return;
        }
        let dir = Dir::new("overmount");
        let guarded = open_checking_path(&dir.archive());

        // Any filesystem on top of the directory hides `a.zip`.
        let mut empty = Vec::new();
        rawzip::ZipArchiveWriter::new(&mut empty).finish().unwrap();
        let other =
            std::env::temp_dir().join(format!("zipfs-guard-{}-overmount.zip", std::process::id()));
        std::fs::write(&other, empty).unwrap();
        let archive = crate::Archive::open(&other).unwrap();
        std::fs::remove_file(&other).unwrap();
        let zipfs = crate::ZipFs::new(archive, crate::Config::default()).unwrap();
        let mut config = fuser::Config::default();
        config.mount_options = vec![fuser::MountOption::RO];
        let session = fuser::Session::new(zipfs, &dir.0, &config)
            .unwrap()
            .spawn()
            .unwrap();
        assert!(!dir.archive().exists());

        let found = guarded.check();
        session.umount_and_join().unwrap();
        assert_eq!(found, Freshness::Current);
    }
}
