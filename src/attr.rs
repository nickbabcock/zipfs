//! Turns index nodes into the attributes the kernel expects.

use crate::config::Config;
use crate::index::{EntryMeta, Index, Node, NodeKind};
use fuser::{FileAttr, FileType, INodeNo};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The block size reported by `stat`.
pub const BLOCK_SIZE: u32 = 4096;

/// Converts a Unix timestamp, clamping anything the platform cannot hold.
fn to_system_time(secs: i64, nanos: u32) -> SystemTime {
    let nanos = Duration::from_nanos(u64::from(nanos.min(999_999_999)));
    let whole = Duration::from_secs(secs.unsigned_abs());
    let base = if secs.is_negative() {
        UNIX_EPOCH.checked_sub(whole)
    } else {
        UNIX_EPOCH.checked_add(whole)
    };

    base.and_then(|time| time.checked_add(nanos))
        .unwrap_or(UNIX_EPOCH)
}

/// Builds the attributes for one inode.
#[must_use]
pub fn file_attr(index: &Index, ino: u64, node: &Node, config: &Config) -> FileAttr {
    let meta = index.meta(node);
    let size = match node.kind {
        NodeKind::Dir => 0,
        _ => meta.map_or(0, |m| m.uncompressed_size),
    };
    let kind = match node.kind {
        NodeKind::Dir => FileType::Directory,
        NodeKind::File => FileType::RegularFile,
        NodeKind::Symlink => FileType::Symlink,
    };
    let perm = permissions(node, meta, config);
    let mtime = meta.map_or(UNIX_EPOCH, |m| to_system_time(m.mtime_sec, m.mtime_nsec));
    let nlink = match node.kind {
        // A directory links to itself, to its parent, and once per subdirectory.
        NodeKind::Dir => 2 + node.subdir_count,
        _ => 1,
    };

    FileAttr {
        ino: INodeNo(ino),
        size,
        // The compressed entry occupies less than its size suggests, but
        // reporting that makes tools believe the file is sparse.
        blocks: size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind,
        perm,
        nlink,
        uid: config.uid,
        gid: config.gid,
        rdev: 0,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

/// The permission bits for a node.
///
/// Archives written on DOS and Windows record no Unix mode at all, so those
/// fall back to the configured defaults. Set-user-id and friends are never
/// honoured: the archive is not a trusted source of them.
fn permissions(node: &Node, meta: Option<&EntryMeta>, config: &Config) -> u16 {
    let recorded = meta.map_or(0, |m| m.mode) & 0o777;
    let mode = if recorded != 0 {
        recorded
    } else if node.kind == NodeKind::Dir {
        config.dir_mode
    } else {
        config.file_mode
    };
    mode & 0o777
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_timestamps_keep_nanoseconds() {
        let expected = UNIX_EPOCH.checked_sub(Duration::from_millis(500)).unwrap();

        assert_eq!(to_system_time(-1, 500_000_000), expected);
    }
}
