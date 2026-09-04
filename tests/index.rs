//! The tree the central directory turns into.

mod common;

use common::{Entry, TempArchive, archive};
use rawzip::CompressionMethod;
const STORE: CompressionMethod = CompressionMethod::STORE;
const DEFLATE: CompressionMethod = CompressionMethod::DEFLATE;
use zipfs::index::{NodeKind, ROOT_INO};
use zipfs::{Archive, Index};

fn index_of(name: &str, entries: &[Entry]) -> (TempArchive, Index, zipfs::index::BuildStats) {
    let temp = TempArchive::new(name, &archive(entries));
    let archive = Archive::open(&temp.path).unwrap();
    let (index, stats) = zipfs::index::build(&archive).unwrap();
    (temp, index, stats)
}

/// Walks a slash separated path from the root.
fn resolve(index: &Index, path: &str) -> Option<u64> {
    let mut ino = ROOT_INO;
    for component in path.split('/').filter(|c| !c.is_empty()) {
        ino = index.lookup(ino, component.as_bytes())?;
    }
    Some(ino)
}

fn names(index: &Index, dir: u64) -> Vec<String> {
    let node = index.node(dir).unwrap();
    index
        .children(node)
        .iter()
        .map(|c| String::from_utf8_lossy(index.name(c)).into_owned())
        .collect()
}

#[test]
fn directories_are_created_for_paths_that_do_not_declare_them() {
    let (_t, index, stats) = index_of("implied", &[Entry::file("a/b/c/deep.txt", "hello", STORE)]);

    let dir = resolve(&index, "a/b/c").expect("the implied directories exist");
    assert_eq!(index.node(dir).unwrap().kind, NodeKind::Dir);
    assert_eq!(names(&index, dir), ["deep.txt"]);
    assert_eq!(stats.entries, 1);
    assert_eq!(stats.synthetic_dirs, 3);
}

#[test]
fn children_are_sorted_and_found_by_lookup() {
    let (_t, index, _) = index_of(
        "sorted",
        &[
            Entry::file("dir/zebra", "z", STORE),
            Entry::file("dir/apple", "a", STORE),
            Entry::file("dir/mango", "m", STORE),
        ],
    );

    let dir = resolve(&index, "dir").unwrap();
    assert_eq!(names(&index, dir), ["apple", "mango", "zebra"]);
    assert!(index.lookup(dir, b"mango").is_some());
    assert!(index.lookup(dir, b"durian").is_none());
}

#[test]
fn a_directory_child_range_covers_only_its_own_children() {
    let (_t, index, _) = index_of(
        "ranges",
        &[
            Entry::file("one/a", "a", STORE),
            Entry::file("one/b", "b", STORE),
            Entry::file("two/c", "c", STORE),
        ],
    );

    assert_eq!(names(&index, ROOT_INO), ["one", "two"]);
    assert_eq!(names(&index, resolve(&index, "one").unwrap()), ["a", "b"]);
    assert_eq!(names(&index, resolve(&index, "two").unwrap()), ["c"]);
}

#[test]
fn the_first_of_two_entries_with_one_name_wins() {
    let bytes = archive(&[
        Entry::file("same.txt", "first", STORE),
        Entry::file("same.txt", "second", DEFLATE),
    ]);
    let temp = TempArchive::new("duplicate", &bytes);
    let archive = Archive::open(&temp.path).unwrap();
    let (index, stats) = zipfs::index::build(&archive).unwrap();

    let ino = resolve(&index, "same.txt").unwrap();
    let meta = index.meta(index.node(ino).unwrap()).unwrap();
    assert_eq!(meta.uncompressed_size, 5);
    assert_eq!(meta.method_raw, 0, "the stored first entry was kept");
    assert_eq!(stats.duplicates, 1);
    assert_eq!(names(&index, ROOT_INO), ["same.txt"]);
}

#[test]
fn a_directory_beats_a_file_of_the_same_name() {
    for order in [0, 1] {
        let mut entries = vec![
            Entry::file("clash", "i am a file", STORE),
            Entry::file("clash/inside.txt", "i am under a directory", STORE),
        ];
        if order == 1 {
            entries.reverse();
        }
        let (_t, index, stats) = index_of("clash", &entries);

        let ino = resolve(&index, "clash").unwrap();
        assert_eq!(index.node(ino).unwrap().kind, NodeKind::Dir);
        assert_eq!(names(&index, ino), ["inside.txt"]);
        assert_eq!(stats.duplicates, 1);
    }
}

#[test]
fn a_path_that_escapes_the_archive_is_skipped() {
    // The writer normalises paths, so a hostile name has to be patched in.
    // The replacement is the same length, which keeps every header offset valid.
    let mut bytes = archive(&[
        Entry::file("aa/bb.txt", "no", STORE),
        Entry::file("fine.txt", "yes", STORE),
    ]);
    replace_all(&mut bytes, b"aa/bb.txt", b"../bb.txt");

    let temp = TempArchive::new("escape", &bytes);
    let archive = Archive::open(&temp.path).unwrap();
    let (index, stats) = zipfs::index::build(&archive).unwrap();

    assert_eq!(names(&index, ROOT_INO), ["fine.txt"]);
    assert_eq!(stats.rejected_paths, 1);
}

/// Overwrites every occurrence of `from` with `to`, which must be the same size.
fn replace_all(haystack: &mut [u8], from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    let mut found = 0;
    for i in 0..=haystack.len() - from.len() {
        if &haystack[i..i + from.len()] == from {
            haystack[i..i + to.len()].copy_from_slice(to);
            found += 1;
        }
    }
    assert!(
        found >= 2,
        "expected a local and a central directory header"
    );
}

#[test]
fn repeated_separators_and_dot_components_collapse() {
    let (_t, index, _) = index_of("collapse", &[Entry::file("a//./b/./file.txt", "hi", STORE)]);

    assert!(resolve(&index, "a/b/file.txt").is_some());
    assert_eq!(names(&index, ROOT_INO), ["a"]);
}

#[test]
fn trailing_separators_and_dot_leaves_are_normalized() {
    let (_t, index, _) = index_of(
        "leaf-normalization",
        &[
            Entry::file("a//", "", STORE),
            Entry::file("b/./", "", STORE),
        ],
    );

    assert_eq!(names(&index, ROOT_INO), ["a", "b"]);
    assert!(names(&index, resolve(&index, "a").unwrap()).is_empty());
    assert!(names(&index, resolve(&index, "b").unwrap()).is_empty());
}

#[test]
fn a_stored_entry_with_different_sizes_is_not_indexed() {
    let mut bytes = archive(&[
        Entry::file("data", "payload", STORE),
        Entry::file("next", "safe", STORE),
    ]);
    patch_central_sizes(&mut bytes, b"data", 1, 7);

    let temp = TempArchive::new("stored-sizes", &bytes);
    let archive = Archive::open(&temp.path).unwrap();
    let (index, stats) = zipfs::index::build(&archive).unwrap();

    assert!(resolve(&index, "data").is_none());
    assert!(resolve(&index, "next").is_some());
    assert_eq!(stats.rejected_sizes, 1);
}

#[test]
fn an_encrypted_stored_entry_with_different_sizes_stays_indexed() {
    let mut bytes = archive(&[Entry::file("secret", "payload", STORE)]);
    patch_central_sizes(&mut bytes, b"secret", 1, 7);
    patch_central_encrypted(&mut bytes, b"secret");

    let temp = TempArchive::new("encrypted-stored", &bytes);
    let archive = Archive::open(&temp.path).unwrap();
    let (index, stats) = zipfs::index::build(&archive).unwrap();

    let ino = resolve(&index, "secret").expect("the encrypted entry is listed");
    let meta = index.meta(index.node(ino).unwrap()).unwrap();
    assert!(meta.encrypted);
    assert_eq!(stats.encrypted, 1);
}

#[test]
fn a_directory_entry_claims_the_node_an_earlier_path_implied() {
    let (_t, index, stats) = index_of(
        "declared",
        &[
            Entry::file("d/inside.txt", "x", STORE),
            Entry::file("d/", "", STORE),
        ],
    );

    let ino = resolve(&index, "d").unwrap();
    let node = index.node(ino).unwrap();
    assert_eq!(node.kind, NodeKind::Dir);
    assert!(index.meta(node).is_some(), "the entry was attached");
    assert_eq!(stats.duplicates, 0);
    assert_eq!(stats.synthetic_dirs, 0);
}

#[test]
fn a_name_that_is_not_text_survives_as_bytes() {
    // Latin-1 bytes are common in archives written on older systems.
    let bytes = archive(&[Entry::file("caf\u{e9}.txt", "x", STORE)]);
    let temp = TempArchive::new("bytes", &bytes);
    let archive = Archive::open(&temp.path).unwrap();
    let (index, _) = zipfs::index::build(&archive).unwrap();

    let children = names(&index, ROOT_INO);
    assert_eq!(children.len(), 1);
    assert!(index.lookup(ROOT_INO, "caf\u{e9}.txt".as_bytes()).is_some());
}

#[test]
fn a_symlink_is_recognised_from_its_mode() {
    let (_t, index, stats) = index_of("symlink", &[Entry::symlink("link", "target.txt")]);

    let ino = resolve(&index, "link").unwrap();
    assert_eq!(index.node(ino).unwrap().kind, NodeKind::Symlink);
    assert_eq!(stats.symlinks, 1);
}

#[test]
fn nested_directories_count_towards_their_parent_link_count() {
    let (_t, index, _) = index_of(
        "nlink",
        &[
            Entry::file("top/one/x", "x", STORE),
            Entry::file("top/two/y", "y", STORE),
            Entry::file("top/file", "f", STORE),
        ],
    );

    let top = index.node(resolve(&index, "top").unwrap()).unwrap();
    assert_eq!(top.subdir_count, 2);
    assert_eq!(top.child_len, 3);
}

#[test]
fn an_empty_archive_still_has_a_root() {
    let (_t, index, stats) = index_of("empty", &[]);
    assert_eq!(index.node(ROOT_INO).unwrap().kind, NodeKind::Dir);
    assert!(names(&index, ROOT_INO).is_empty());
    assert_eq!(stats.entries, 0);
}

fn patch_central_sizes(bytes: &mut [u8], name: &[u8], compressed: u32, uncompressed: u32) {
    const CENTRAL_HEADER: &[u8; 4] = b"PK\x01\x02";
    const CENTRAL_FIXED: usize = 46;
    const COMPRESSED_SIZE: usize = 20;
    const UNCOMPRESSED_SIZE: usize = 24;
    const NAME_LENGTH: usize = 28;
    const NAME: usize = 46;

    for offset in 0..bytes.len().saturating_sub(CENTRAL_FIXED) {
        if &bytes[offset..offset + 4] != CENTRAL_HEADER {
            continue;
        }
        let name_len = u16::from_le_bytes(
            bytes[offset + NAME_LENGTH..offset + NAME_LENGTH + 2]
                .try_into()
                .unwrap(),
        ) as usize;
        if name_len == name.len() && bytes[offset + NAME..offset + NAME + name_len] == *name {
            bytes[offset + COMPRESSED_SIZE..offset + COMPRESSED_SIZE + 4]
                .copy_from_slice(&compressed.to_le_bytes());
            bytes[offset + UNCOMPRESSED_SIZE..offset + UNCOMPRESSED_SIZE + 4]
                .copy_from_slice(&uncompressed.to_le_bytes());
            return;
        }
    }
    panic!("central directory entry not found");
}

fn patch_central_encrypted(bytes: &mut [u8], name: &[u8]) {
    const CENTRAL_HEADER: &[u8; 4] = b"PK\x01\x02";
    const CENTRAL_FIXED: usize = 46;
    const FLAGS: usize = 8;
    const NAME_LENGTH: usize = 28;
    const NAME: usize = 46;

    for offset in 0..bytes.len().saturating_sub(CENTRAL_FIXED) {
        if &bytes[offset..offset + 4] != CENTRAL_HEADER {
            continue;
        }
        let name_len = u16::from_le_bytes(
            bytes[offset + NAME_LENGTH..offset + NAME_LENGTH + 2]
                .try_into()
                .unwrap(),
        ) as usize;
        if name_len == name.len() && bytes[offset + NAME..offset + NAME + name_len] == *name {
            bytes[offset + FLAGS] |= 1;
            return;
        }
    }
    panic!("central directory entry not found");
}
