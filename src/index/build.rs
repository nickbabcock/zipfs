//! Builds the tree from the archive's central directory.
//!
//! Two passes. The first walks the central directory once, validating paths and
//! interning every component, which discovers nodes in archive order. The second
//! sorts siblings and relabels the tree breadth first, which is what makes each
//! directory's children contiguous.

use super::intern::{Interner, TmpNode};
use super::{EntryMeta, FLAG_SYNTHETIC, Index, NO_META, Node, NodeKind, VERIFY_UNKNOWN};
use crate::archive::Archive;
use log::{debug, warn};
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64};

/// The largest number of entries preallocated for.
///
/// The entry count in the archive is a hint from an untrusted source, so it
/// cannot be used to size an allocation directly.
const MAX_PREALLOC: u64 = 1 << 20;
/// How many path components deep an entry may be.
const MAX_DEPTH: usize = 256;
/// The longest single path component, matching `NAME_MAX`.
const MAX_COMPONENT: usize = 255;
/// How many of each kind of complaint to log before falling back to a count.
const MAX_WARNINGS: u32 = 16;

/// What the build had to skip or work around.
#[derive(Debug, Default, Clone)]
pub struct BuildStats {
    /// Entries indexed, not counting synthetic directories.
    pub entries: u64,
    /// Entries dropped because their path was unusable.
    pub rejected_paths: u64,
    /// Entries dropped because their compressed length was not trustworthy.
    pub rejected_sizes: u64,
    /// Entries that lost a name to an earlier entry.
    pub duplicates: u64,
    /// Entries kept but not readable because they are encrypted.
    pub encrypted: u64,
    /// Entries kept but not readable because of their compression method.
    pub unsupported: u64,
    /// Directories invented because no entry declared them.
    pub synthetic_dirs: u64,
    /// Symlinks found.
    pub symlinks: u64,
}

struct Warner {
    seen: u32,
}

impl Warner {
    fn new() -> Warner {
        Warner { seen: 0 }
    }

    /// True while complaints should still be logged one by one.
    fn allow(&mut self) -> bool {
        self.seen += 1;
        self.seen <= MAX_WARNINGS
    }
}

/// Renders a path for a log line without assuming it is text.
fn show(path: &[u8]) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path {
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            write!(&mut out, "\\x{b:02x}").expect("writing to a String cannot fail");
        }
    }
    out
}

/// Why a path cannot become a node.
fn reject_reason(path: &[u8]) -> Option<&'static str> {
    if path.is_empty() {
        return Some("empty path");
    }
    if path.contains(&0) {
        return Some("path contains a NUL byte");
    }
    if path[0] == b'/' || path[0] == b'\\' {
        return Some("absolute path");
    }
    if path.len() >= 2 && path[1] == b':' && path[0].is_ascii_alphabetic() {
        return Some("path has a drive prefix");
    }
    let mut depth = 0;
    for comp in components(path) {
        if comp == b".." {
            return Some("path escapes the archive root");
        }
        if comp.len() > MAX_COMPONENT {
            return Some("path component is too long");
        }
        depth += 1;
        if depth > MAX_DEPTH {
            return Some("path is too deep");
        }
    }
    if depth == 0 {
        return Some("path has no components");
    }
    None
}

/// The meaningful components of a path.
///
/// Repeated separators collapse and `.` components drop out, which matches how
/// the path would be resolved if it were extracted.
fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
}

/// Splits a path into the bytes naming its parent and its own last component.
///
/// The parent part keeps its exact bytes so it can be compared against the
/// previous entry's, which is what lets consecutive entries in the same
/// directory skip the interner entirely.
fn split_parent(path: &[u8]) -> (&[u8], &[u8]) {
    let body = match path.split_last() {
        Some((b'/', rest)) => rest,
        _ => path,
    };
    match body.iter().rposition(|&b| b == b'/') {
        Some(i) => (&body[..=i], &body[i + 1..]),
        None => (&body[..0], body),
    }
}

/// Returns one canonical path made from the meaningful components.
fn normalize_path(path: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(path.len());
    for component in components(path) {
        if !normalized.is_empty() {
            normalized.push(b'/');
        }
        normalized.extend_from_slice(component);
    }
    normalized
}

struct Builder {
    nodes: Vec<TmpNode>,
    names: Vec<u8>,
    metas: Vec<EntryMeta>,
    intern: Interner,
    /// The parent directory the previous entry resolved to.
    memo_path: Vec<u8>,
    memo_id: u32,
    symlink_count: u32,
    stats: BuildStats,
    dup_warner: Warner,
    reject_warner: Warner,
}

impl Builder {
    fn new(hint: usize) -> Builder {
        let mut nodes = Vec::with_capacity(hint + 1);
        // The root. It is its own parent, which is what `..` should report.
        nodes.push(TmpNode {
            name_off: 0,
            name_len: 0,
            kind: NodeKind::Dir,
            flags: FLAG_SYNTHETIC,
            parent: 0,
            meta: NO_META,
        });
        Builder {
            nodes,
            names: Vec::with_capacity(hint * 16),
            metas: Vec::with_capacity(hint),
            intern: Interner::with_capacity(hint),
            memo_path: Vec::new(),
            memo_id: 0,
            symlink_count: 0,
            stats: BuildStats::default(),
            dup_warner: Warner::new(),
            reject_warner: Warner::new(),
        }
    }

    /// Interns a component under a parent, appending it to the name blob only
    /// if it is new.
    fn intern(&mut self, parent: u32, name: &[u8]) -> crate::Result<(u32, bool)> {
        if let Some(id) = self.intern.find(&self.nodes, &self.names, parent, name) {
            return Ok((id, false));
        }
        let name_off = u32::try_from(self.names.len())
            .map_err(|_error| crate::Error::IndexTooLarge("name blob"))?;
        self.names.extend_from_slice(name);
        let id = u32::try_from(self.nodes.len())
            .map_err(|_error| crate::Error::IndexTooLarge("node count"))?;
        self.nodes.push(TmpNode {
            name_off,
            name_len: u16::try_from(name.len())
                .map_err(|_error| crate::Error::IndexTooLarge("path component length"))?,
            kind: NodeKind::Dir,
            flags: FLAG_SYNTHETIC,
            parent,
            meta: NO_META,
        });
        self.intern.insert(&self.nodes, &self.names, id);
        Ok((id, true))
    }

    /// Resolves the directory a path lives in, creating what is missing.
    fn resolve_parent(&mut self, parent_path: &[u8]) -> crate::Result<u32> {
        if parent_path == self.memo_path.as_slice() {
            return Ok(self.memo_id);
        }
        let mut cur = 0u32;
        for comp in components(parent_path) {
            let (id, fresh) = self.intern(cur, comp)?;
            if fresh {
                self.stats.synthetic_dirs += 1;
            } else if self.nodes[id as usize].kind != NodeKind::Dir {
                // A path needs this to be a directory but an entry claimed it
                // as a file. The directory wins, because dropping it would
                // orphan everything underneath.
                self.take_over_as_dir(id, comp);
            }
            cur = id;
        }
        self.memo_path.clear();
        self.memo_path.extend_from_slice(parent_path);
        self.memo_id = cur;
        Ok(cur)
    }

    /// Turns a node that was indexed as a file into a directory.
    fn take_over_as_dir(&mut self, id: u32, name: &[u8]) {
        if self.dup_warner.allow() {
            warn!(
                "{}: indexed as both a file and a directory; keeping the directory",
                show(name)
            );
        }
        let node = &mut self.nodes[id as usize];
        node.kind = NodeKind::Dir;
        node.flags |= FLAG_SYNTHETIC;
        node.meta = NO_META;
        self.stats.duplicates += 1;
        self.stats.synthetic_dirs += 1;
    }

    fn reject(&mut self, path: &[u8], why: &str) {
        if self.reject_warner.allow() {
            warn!("{}: skipped, {}", show(path), why);
        }
        self.stats.rejected_paths += 1;
    }

    fn add(&mut self, path: &[u8], is_dir: bool, meta: EntryMeta) -> crate::Result<()> {
        if let Some(why) = reject_reason(path) {
            self.reject(path, why);
            return Ok(());
        }
        // A stored entry has no decoder. Its two sizes must define the same
        // byte range.
        if !meta.encrypted
            && meta.method_raw == rawzip::CompressionMethod::STORE.as_u16()
            && meta.compressed_size != meta.uncompressed_size
        {
            if self.reject_warner.allow() {
                warn!("{}: skipped, stored sizes differ", show(path));
            }
            self.stats.rejected_sizes += 1;
            return Ok(());
        }
        // Without a trustworthy compressed length there is no safe place to
        // stop reading, so such an entry is not indexed at all.
        if meta.uncompressed_size > 0 && meta.compressed_size == 0 {
            if self.reject_warner.allow() {
                warn!("{}: skipped, compressed size is zero", show(path));
            }
            self.stats.rejected_sizes += 1;
            return Ok(());
        }

        let normalized = normalize_path(path);
        let (parent_path, leaf) = split_parent(&normalized);
        let parent = self.resolve_parent(parent_path)?;
        let (id, fresh) = self.intern(parent, leaf)?;

        let kind = if is_dir {
            NodeKind::Dir
        } else if is_symlink_mode(meta.mode) {
            NodeKind::Symlink
        } else {
            NodeKind::File
        };

        if !fresh {
            let existing = self.nodes[id as usize];
            let claimed = existing.flags & FLAG_SYNTHETIC == 0;
            if existing.kind == NodeKind::Dir && kind != NodeKind::Dir {
                // The directory wins over a file of the same name.
                if self.dup_warner.allow() {
                    warn!(
                        "{}: indexed as both a file and a directory; keeping the directory",
                        show(path)
                    );
                }
                self.stats.duplicates += 1;
                return Ok(());
            }
            if claimed {
                // First entry wins.
                if self.dup_warner.allow() {
                    warn!("{}: duplicate entry, keeping the first", show(path));
                }
                self.stats.duplicates += 1;
                return Ok(());
            }
            // The node exists only because a deeper path needed it. This entry
            // is the real declaration of that directory, so attach it.
            if kind != NodeKind::Dir {
                self.stats.duplicates += 1;
                return Ok(());
            }
            self.stats.synthetic_dirs -= 1;
        }

        self.stats.entries += 1;
        if meta.encrypted {
            self.stats.encrypted += 1;
        } else if meta.method().is_none() {
            self.stats.unsupported += 1;
        }

        let mut meta = meta;
        if kind == NodeKind::Symlink {
            meta.symlink_slot = self.symlink_count;
            self.symlink_count = self
                .symlink_count
                .checked_add(1)
                .ok_or(crate::Error::IndexTooLarge("symlink count"))?;
            self.stats.symlinks += 1;
        }
        let meta_idx = u32::try_from(self.metas.len())
            .map_err(|_error| crate::Error::IndexTooLarge("metadata count"))?;
        self.metas.push(meta);

        let node = &mut self.nodes[id as usize];
        node.kind = kind;
        node.flags &= !FLAG_SYNTHETIC;
        node.meta = meta_idx;
        Ok(())
    }

    /// Sorts siblings and relabels the tree so children are contiguous.
    fn finish(self) -> crate::Result<Index> {
        let Builder {
            nodes: tmp,
            names,
            metas,
            symlink_count,
            ..
        } = self;
        let (starts, kids) = group_children(&tmp, &names)?;
        let (order, first_child, final_of) = breadth_first_order(&tmp, &starts, &kids)?;

        let n = tmp.len();
        // Index 0 is never a valid inode, so it holds a dead node and the root
        // lands at index 1 where FUSE expects it.
        let mut nodes = Vec::with_capacity(n + 1);
        nodes.push(Node {
            name_off: 0,
            name_len: 0,
            kind: NodeKind::Dir,
            flags: FLAG_SYNTHETIC,
            parent: 1,
            first_child: 0,
            child_len: 0,
            subdir_count: 0,
            meta: NO_META,
        });
        let mut total_uncompressed = 0u64;
        for &tid in &order {
            let t = &tmp[tid as usize];
            let (lo, hi) = (
                starts[tid as usize] as usize,
                starts[tid as usize + 1] as usize,
            );
            let subdir_count = kids[lo..hi]
                .iter()
                .filter(|&&k| tmp[k as usize].kind == NodeKind::Dir)
                .count();
            let subdir_count = u32::try_from(subdir_count)
                .map_err(|_error| crate::Error::IndexTooLarge("subdirectory count"))?;
            if t.meta != NO_META {
                total_uncompressed =
                    total_uncompressed.saturating_add(metas[t.meta as usize].uncompressed_size);
            }
            nodes.push(Node {
                name_off: t.name_off,
                name_len: t.name_len,
                kind: t.kind,
                flags: t.flags,
                parent: if tid == 0 {
                    1
                } else {
                    final_of[t.parent as usize]
                },
                first_child: first_child[tid as usize],
                child_len: u32::try_from(hi - lo)
                    .map_err(|_error| crate::Error::IndexTooLarge("child count"))?,
                subdir_count,
                meta: t.meta,
            });
        }

        let symlinks = (0..symlink_count).map(|_| OnceLock::new()).collect();

        Ok(Index {
            nodes: nodes.into_boxed_slice(),
            metas: metas.into_boxed_slice(),
            names: names.into_boxed_slice(),
            symlinks,
            total_uncompressed,
        })
    }
}

/// Groups child nodes by parent and sorts each group by name.
fn group_children(tmp: &[TmpNode], names: &[u8]) -> crate::Result<(Vec<u32>, Vec<u32>)> {
    let n = tmp.len();
    let mut counts = vec![0u32; n + 1];
    for (i, node) in tmp.iter().enumerate() {
        if i != 0 {
            let count = &mut counts[node.parent as usize + 1];
            *count = count
                .checked_add(1)
                .ok_or(crate::Error::IndexTooLarge("child count"))?;
        }
    }
    for i in 0..n {
        counts[i + 1] = counts[i + 1]
            .checked_add(counts[i])
            .ok_or(crate::Error::IndexTooLarge("node count"))?;
    }
    let starts = counts.clone();
    let mut cursor = counts;
    let mut kids = vec![0u32; n.saturating_sub(1)];
    for (i, node) in tmp.iter().enumerate().skip(1) {
        let slot = &mut cursor[node.parent as usize];
        kids[*slot as usize] =
            u32::try_from(i).map_err(|_error| crate::Error::IndexTooLarge("node count"))?;
        *slot = (*slot)
            .checked_add(1)
            .ok_or(crate::Error::IndexTooLarge("child count"))?;
    }

    let name_of = |id: u32| -> &[u8] {
        let t = &tmp[id as usize];
        &names[t.name_off as usize..t.name_off as usize + t.name_len as usize]
    };
    for i in 0..n {
        let (lo, hi) = (starts[i] as usize, starts[i + 1] as usize);
        kids[lo..hi].sort_unstable_by(|&a, &b| name_of(a).cmp(name_of(b)));
    }
    Ok((starts, kids))
}

/// Relabels nodes in breadth-first order so children are contiguous.
fn breadth_first_order(
    tmp: &[TmpNode],
    starts: &[u32],
    kids: &[u32],
) -> crate::Result<(Vec<u32>, Vec<u32>, Vec<u32>)> {
    let n = tmp.len();
    let mut order: Vec<u32> = Vec::with_capacity(n);
    let mut final_of = vec![0u32; n];
    order.push(0);
    final_of[0] = 1;
    let mut first_child = vec![0u32; n];
    let mut i = 0;
    while i < order.len() {
        let t = order[i] as usize;
        i += 1;
        let (lo, hi) = (starts[t] as usize, starts[t + 1] as usize);
        first_child[t] = u32::try_from(order.len() + 1)
            .map_err(|_error| crate::Error::IndexTooLarge("node count"))?;
        for &k in &kids[lo..hi] {
            final_of[k as usize] = u32::try_from(order.len() + 1)
                .map_err(|_error| crate::Error::IndexTooLarge("node count"))?;
            order.push(k);
        }
    }
    Ok((order, first_child, final_of))
}

fn is_symlink_mode(mode: u16) -> bool {
    u32::from(mode) & libc::S_IFMT == libc::S_IFLNK
}

/// Reads the whole central directory and builds the tree.
///
/// # Errors
///
/// Returns an error when the archive central directory cannot be read.
pub fn build(archive: &Archive) -> crate::Result<(Index, BuildStats)> {
    let hint = archive.entries_hint().min(MAX_PREALLOC) as usize;
    let mut builder = Builder::new(hint);
    let mut buf = vec![0u8; rawzip::RECOMMENDED_BUFFER_SIZE];
    let mut entries = archive.zip().entries(&mut buf);
    let mut counted = 0u64;

    while let Some(record) = entries.next_entry()? {
        counted += 1;
        // The record borrows the scan buffer, so everything needed has to be
        // copied out before the next call.
        let path = record.file_path();
        let path = path.as_ref();
        let mode = record.mode();
        let (mtime_sec, mtime_nanos) = to_unix(&record.last_modified());
        let meta = EntryMeta {
            wayfinder: record.wayfinder(),
            uncompressed_size: record.uncompressed_size_hint(),
            compressed_size: record.compressed_size_hint(),
            mtime_sec,
            mtime_nsec: mtime_nanos,
            crc32: record.crc32(),
            method_raw: record.compression_method().as_u16(),
            mode: (mode.value() & 0xffff) as u16,
            encrypted: record.flags().is_encrypted(),
            data_start: AtomicU64::new(u64::MAX),
            verify_state: AtomicU8::new(VERIFY_UNKNOWN),
            symlink_slot: 0,
        };
        builder.add(path, record.is_dir(), meta)?;
    }

    let hinted = archive.entries_hint();
    if counted != hinted {
        debug!("central directory declared {hinted} entries but yielded {counted}");
    }

    let mut stats = std::mem::take(&mut builder.stats);
    let index = builder.finish()?;
    stats.synthetic_dirs = index
        .nodes
        .iter()
        .skip(2)
        .filter(|n| n.flags & FLAG_SYNTHETIC != 0)
        .count() as u64;
    Ok((index, stats))
}

/// Converts a zip timestamp to seconds and nanoseconds since the Unix epoch.
///
/// A local timestamp carries no offset, so there is nothing better to do than
/// read it as UTC.
fn to_unix(dt: &rawzip::time::ZipDateTimeKind) -> (i64, u32) {
    let days = days_from_civil(i64::from(dt.year()), dt.month(), dt.day());
    let secs = days * 86_400
        + i64::from(dt.hour()) * 3_600
        + i64::from(dt.minute()) * 60
        + i64::from(dt.second());
    (secs, dt.nanosecond())
}

/// Days between the Unix epoch and a civil date, after Howard Hinnant.
fn days_from_civil(year: i64, month: u8, day: u8) -> i64 {
    let (month, day) = (i64::from(month.max(1)), i64::from(day.max(1)));
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_shifted = (month + 9) % 12;
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(path: &str) -> Vec<&[u8]> {
        components(path.as_bytes()).collect()
    }

    #[test]
    fn well_formed_paths_are_accepted() {
        for path in ["a", "a/b", "a/b/c.txt", "dir/", "weird\\name", "a b/c d"] {
            assert!(reject_reason(path.as_bytes()).is_none(), "rejected {path}");
        }
    }

    #[test]
    fn paths_that_could_escape_the_archive_are_rejected() {
        for path in ["", "..", "../x", "a/../b", "a/..", "/abs", "\\abs", "C:/x"] {
            assert!(reject_reason(path.as_bytes()).is_some(), "accepted {path}");
        }
    }

    #[test]
    fn paths_that_cannot_become_names_are_rejected() {
        assert!(reject_reason(b"a\0b").is_some());
        assert!(reject_reason(b"/").is_some());
        assert!(reject_reason(b"./").is_some());
        let long = vec![b'x'; MAX_COMPONENT + 1];
        assert!(reject_reason(&long).is_some());
        let deep: Vec<u8> = std::iter::repeat_n(b"a".as_slice(), MAX_DEPTH + 1)
            .collect::<Vec<_>>()
            .join(&b'/');
        assert!(reject_reason(&deep).is_some());
    }

    #[test]
    fn repeated_separators_and_dot_components_drop_out() {
        assert_eq!(parts("a//b"), [b"a".as_slice(), b"b"]);
        assert_eq!(parts("./a/./b/"), [b"a".as_slice(), b"b"]);
        assert_eq!(parts("a"), [b"a".as_slice()]);
    }

    #[test]
    fn a_path_splits_into_its_parent_and_its_last_component() {
        assert_eq!(
            split_parent(b"a/b/c"),
            (b"a/b/".as_slice(), b"c".as_slice())
        );
        assert_eq!(split_parent(b"a/b/"), (b"a/".as_slice(), b"b".as_slice()));
        assert_eq!(split_parent(b"c"), (b"".as_slice(), b"c".as_slice()));
        assert_eq!(split_parent(b"c/"), (b"".as_slice(), b"c".as_slice()));
    }

    #[test]
    fn timestamps_convert_to_the_unix_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(1980, 1, 1), 3652);
    }
}
