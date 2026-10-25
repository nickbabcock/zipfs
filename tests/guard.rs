//! Tests that find a change to the archive underneath the mount.

mod common;

use common::{Entry, TempArchive, archive};
use rawzip::CompressionMethod;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use zipfs::{Archive, Freshness};

const STORE: CompressionMethod = CompressionMethod::STORE;

fn first() -> Vec<u8> {
    archive(&[Entry::file("a", "first", STORE)])
}

fn second() -> Vec<u8> {
    archive(&[
        Entry::file("b", "second", STORE),
        Entry::file("c", "more", STORE),
    ])
}

#[test]
fn an_unchanged_archive_is_not_stale() {
    let temp = TempArchive::new("guard-same", &first());
    let archive = Archive::open(&temp.path).unwrap();
    assert_eq!(archive.check(), Freshness::Current);
    assert!(!archive.reader().is_stale());
}

#[test]
fn an_archive_written_over_in_place_is_stale() {
    let temp = TempArchive::new("guard-inplace", &first());
    let archive = Archive::open(&temp.path).unwrap();
    fs::write(&temp.path, second()).unwrap();
    assert_eq!(archive.check(), Freshness::Stale);
    assert!(archive.reader().is_stale());
}

#[test]
fn an_archive_replaced_by_rename_is_not_stale_on_a_local_filesystem() {
    let temp = TempArchive::new("guard-rename-local", &first());
    let archive = Archive::open(&temp.path).unwrap();
    let next = TempArchive::new("guard-rename-local-next", &second());
    fs::rename(&next.path, &temp.path).unwrap();
    // The descriptor keeps the file that was opened, and it did not change.
    assert_eq!(archive.check(), Freshness::Current);
}

#[test]
fn a_removed_archive_is_not_stale_on_a_local_filesystem() {
    let temp = TempArchive::new("guard-removed-local", &first());
    let archive = Archive::open(&temp.path).unwrap();
    fs::remove_file(&temp.path).unwrap();
    assert_eq!(archive.check(), Freshness::Current);
}

#[test]
fn a_change_to_the_metadata_only_is_not_stale() {
    let temp = TempArchive::new("guard-chmod", &first());
    let archive = Archive::open(&temp.path).unwrap();
    let mut perms = fs::metadata(&temp.path).unwrap().permissions();
    perms.set_mode(0o600);
    fs::set_permissions(&temp.path, perms).unwrap();
    let link = temp.path.with_extension("link");
    fs::hard_link(&temp.path, &link).unwrap();
    fs::remove_file(&link).unwrap();
    assert_eq!(archive.check(), Freshness::Current);
}

#[test]
fn a_change_stays_found() {
    let temp = TempArchive::new("guard-latch", &first());
    let archive = Archive::open(&temp.path).unwrap();
    let original = fs::read(&temp.path).unwrap();
    fs::write(&temp.path, second()).unwrap();
    assert_eq!(archive.check(), Freshness::Stale);
    // When the old bytes come back, the index is not correct again, because
    // a read in between may already have cached the wrong data.
    fs::write(&temp.path, original).unwrap();
    assert_eq!(archive.check(), Freshness::Stale);
}
