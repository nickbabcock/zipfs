//! The immutable directory tree built from the central directory.
//!
//! Every inode number is a direct index into [`Index::nodes`], so a lookup is an
//! array access. `nodes[0]` is a dead sentinel, which puts the root at index 1
//! where FUSE expects it and removes all offset arithmetic.
//!
//! The tree never changes after it is built, so `forget` has nothing to do and
//! the generation number is always zero.

mod build;
mod intern;

pub(crate) use build::show;
pub use build::{BuildStats, build};

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// The inode of the root directory. FUSE fixes this value.
pub const ROOT_INO: u64 = 1;
/// Marks a node that has no entry in the archive.
pub const NO_META: u32 = u32::MAX;
/// The largest symlink target that is accepted.
pub const MAX_SYMLINK: usize = 4096;

/// What a node is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum NodeKind {
    Dir = 0,
    File = 1,
    Symlink = 2,
}

/// A compression method this filesystem can read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Method {
    Store = 0,
    Deflate = 1,
    #[cfg(feature = "zstd")]
    Zstd = 2,
    Deflate64 = 3,
}

impl Method {
    /// Maps a raw method value, or `None` if this build cannot read it.
    #[must_use]
    pub fn from_raw(raw: u16) -> Option<Method> {
        match rawzip::CompressionMethod::new(raw) {
            rawzip::CompressionMethod::STORE => Some(Method::Store),
            rawzip::CompressionMethod::DEFLATE => Some(Method::Deflate),
            rawzip::CompressionMethod::DEFLATE64 => Some(Method::Deflate64),
            #[cfg(feature = "zstd")]
            rawzip::CompressionMethod::ZSTD | rawzip::CompressionMethod::ZSTD_DEPRECATED => {
                Some(Method::Zstd)
            }
            _ => None,
        }
    }
}

/// The node has no entry of its own; it exists because a deeper path needs it.
pub const FLAG_SYNTHETIC: u8 = 1 << 0;

/// The result of a completed CRC check, remembered so it is done only once.
pub const VERIFY_UNKNOWN: u8 = 0;
pub const VERIFY_OK: u8 = 1;
pub const VERIFY_BAD: u8 = 2;

/// One name in the tree.
#[derive(Clone, Copy, Debug)]
pub struct Node {
    /// Where this node's own name component starts in [`Index::names`].
    pub name_off: u32,
    /// The length of that component. Paths are stored one component at a time,
    /// because a full path repeats every prefix and lookups only ever compare
    /// a single component.
    pub name_len: u16,
    pub kind: NodeKind,
    pub flags: u8,
    /// The containing directory. The root is its own parent.
    pub parent: u32,
    /// Directories only: the inode of the first child. Children of a directory
    /// are contiguous and sorted by name.
    pub first_child: u32,
    /// Directories only: how many children follow `first_child`.
    pub child_len: u32,
    /// Directories only: how many of those children are directories.
    pub subdir_count: u32,
    /// Index into [`Index::metas`], or [`NO_META`].
    pub meta: u32,
}

/// Everything needed to read one archive entry.
#[derive(Debug)]
pub struct EntryMeta {
    /// rawzip's cross-thread handle to the entry. Copy, Send and Sync.
    pub wayfinder: rawzip::ZipArchiveEntryWayfinder,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub crc32: u32,
    /// The raw compression method, kept so an unsupported one can be named.
    pub method_raw: u16,
    pub mode: u16,
    pub encrypted: bool,
    /// Where the compressed bytes start, or `u64::MAX` until the local header
    /// has been read. See [`crate::Archive::data_start`].
    pub data_start: AtomicU64,
    /// One of the `VERIFY_*` values. Sticky once it is not `UNKNOWN`.
    pub verify_state: AtomicU8,
    /// Directories only: the ordinal of this symlink in [`Index::symlinks`].
    pub symlink_slot: u32,
}

impl EntryMeta {
    /// The method, or `None` if this build cannot read it.
    pub fn method(&self) -> Option<Method> {
        Method::from_raw(self.method_raw)
    }

    pub fn verify_state(&self) -> u8 {
        self.verify_state.load(Ordering::Relaxed)
    }

    pub fn set_verify_state(&self, state: u8) {
        self.verify_state.store(state, Ordering::Relaxed);
    }
}

/// The whole directory tree, immutable once built.
#[derive(Debug)]
pub struct Index {
    pub(crate) nodes: Box<[Node]>,
    pub(crate) metas: Box<[EntryMeta]>,
    pub(crate) names: Box<[u8]>,
    pub(crate) symlinks: Box<[OnceLock<Box<[u8]>>]>,
    pub(crate) total_uncompressed: u64,
}

impl Index {
    /// The node for an inode, or `None` if the inode does not exist.
    #[inline]
    #[must_use]
    pub fn node(&self, ino: u64) -> Option<&Node> {
        if ino == 0 {
            return None;
        }
        self.nodes.get(usize::try_from(ino).ok()?)
    }

    /// The entry behind a node, if it has one.
    #[inline]
    #[must_use]
    pub fn meta(&self, node: &Node) -> Option<&EntryMeta> {
        if node.meta == NO_META {
            None
        } else {
            self.metas.get(node.meta as usize)
        }
    }

    /// A node's own name component.
    #[inline]
    #[must_use]
    pub fn name(&self, node: &Node) -> &[u8] {
        let start = node.name_off as usize;
        &self.names[start..start + node.name_len as usize]
    }

    /// A directory's children, sorted by name.
    #[inline]
    #[must_use]
    pub fn children(&self, node: &Node) -> &[Node] {
        if node.kind != NodeKind::Dir {
            return &[];
        }
        let start = node.first_child as usize;
        &self.nodes[start..start + node.child_len as usize]
    }

    /// The inode of a directory's first child.
    #[inline]
    #[must_use]
    pub fn first_child_ino(&self, node: &Node) -> u64 {
        u64::from(node.first_child)
    }

    /// Finds a name in a directory.
    ///
    /// Children are contiguous and sorted, so this is a binary search over a
    /// slice with no extra memory behind it.
    #[must_use]
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Option<u64> {
        let dir = self.node(parent)?;
        if dir.kind != NodeKind::Dir {
            return None;
        }
        let start = dir.first_child as usize;
        let kids = &self.nodes[start..start + dir.child_len as usize];
        let idx = kids.binary_search_by(|c| self.name(c).cmp(name)).ok()?;
        Some((start + idx) as u64)
    }

    /// The number of nodes, including the root and the sentinel.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 2
    }

    /// The sum of every entry's uncompressed size, reported by `statfs`.
    #[must_use]
    pub fn total_uncompressed(&self) -> u64 {
        self.total_uncompressed
    }

    /// The directories above a node, from its parent up. The root is not
    /// included.
    fn ancestors(&self, node: &Node) -> impl Iterator<Item = &Node> {
        let mut ino = u64::from(node.parent);
        std::iter::from_fn(move || {
            if ino == ROOT_INO {
                return None;
            }
            let parent = self.node(ino)?;
            ino = u64::from(parent.parent);
            Some(parent)
        })
    }

    /// The number of directories between a node and the root.
    pub(crate) fn depth(&self, node: &Node) -> usize {
        self.ancestors(node).count()
    }

    /// The path of a node from the root, for a log line.
    pub(crate) fn path(&self, node: &Node) -> Vec<u8> {
        let mut names = vec![self.name(node)];
        names.extend(self.ancestors(node).map(|parent| self.name(parent)));
        names.reverse();
        names.join(&b'/')
    }

    /// The cached target of a symlink, if it has been read.
    #[must_use]
    pub fn symlink(&self, slot: u32) -> Option<&[u8]> {
        self.symlinks.get(slot as usize)?.get().map(|t| &**t)
    }

    /// Caches a symlink target. A racing writer keeps its own value; both
    /// resolved the same entry, so either is correct.
    #[must_use]
    pub fn set_symlink(&self, slot: u32, target: Box<[u8]>) -> Option<&[u8]> {
        let cell = self.symlinks.get(slot as usize)?;
        let _ = cell.set(target);
        cell.get().map(|t| &**t)
    }
}

/// Tells if a symlink target is sure to stay inside the mount.
///
/// `depth` is the number of directories between the link and the root. The
/// kernel starts a relative target at the real directory of the link, so this
/// count is exact for each `..` that comes before all names.
///
/// A `..` after a name is refused. That name can be a symlink to a different
/// directory, so the path does not show where the `..` goes. For example,
/// `a/up -> ..` is the root, so `a/b/esc -> ../up/../..` goes above the root
/// but looks contained if the path alone is resolved. If no `..` follows a
/// name, each link stays inside the mount, whichever links its names go
/// through.
pub(crate) fn symlink_is_contained(target: &[u8], depth: usize) -> bool {
    if target.is_empty() || target.starts_with(b"/") || target.contains(&0) {
        return false;
    }
    let mut ascents = 0;
    let mut after_name = false;
    for component in target.split(|&b| b == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                ascents += 1;
                if after_name || ascents > depth {
                    return false;
                }
            }
            _ => after_name = true,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{Index, NO_META, Node, NodeKind, symlink_is_contained};

    fn node(name_off: u32, name_len: u16, parent: u32) -> Node {
        Node {
            name_off,
            name_len,
            kind: NodeKind::Dir,
            flags: 0,
            parent,
            first_child: 0,
            child_len: 0,
            subdir_count: 0,
            meta: NO_META,
        }
    }

    #[test]
    fn a_node_knows_its_path_and_depth() {
        let index = Index {
            // Inode 0 does not exist, and inode 1 is the root.
            nodes: Box::new([
                node(0, 0, 0),
                node(0, 0, 1),
                node(0, 1, 1),
                node(1, 1, 2),
                node(2, 4, 3),
            ]),
            metas: Box::new([]),
            names: Box::from(&b"ablink"[..]),
            symlinks: Box::new([]),
            total_uncompressed: 0,
        };

        let top = index.node(2).unwrap();
        assert_eq!(index.path(top), b"a");
        assert_eq!(index.depth(top), 0);

        let deep = index.node(4).unwrap();
        assert_eq!(index.path(deep), b"a/b/link");
        assert_eq!(index.depth(deep), 2);
    }

    #[test]
    fn a_target_that_stays_inside_the_mount_is_contained() {
        for (target, depth) in [
            ("target", 0),
            ("sub/target", 0),
            ("./target", 0),
            ("sub//target/", 0),
            (".", 0),
            ("..", 1),
            ("../target", 1),
            ("./../target", 1),
            ("../../sub/target", 2),
            ("../../sub/target", 5),
        ] {
            assert!(
                symlink_is_contained(target.as_bytes(), depth),
                "{target} at depth {depth}"
            );
        }
    }

    #[test]
    fn a_target_that_can_leave_the_mount_is_not_contained() {
        for (target, depth) in [
            ("", 0),
            ("/etc/passwd", 5),
            ("..", 0),
            ("../target", 0),
            ("../../target", 1),
            ("sub/..", 5),
            ("sub/../target", 5),
            ("../up/../..", 5),
            ("a\0b", 5),
        ] {
            assert!(
                !symlink_is_contained(target.as_bytes(), depth),
                "{target} at depth {depth}"
            );
        }
    }
}
