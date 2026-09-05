//! Runs the binary the way `mount(8)` and a script do.
//!
//! Like the other mount tests, these report and pass where FUSE is missing.

mod common;

use common::{Entry, TempArchive, archive};
use rawzip::CompressionMethod;
use std::path::{Path, PathBuf};
use std::process::Command;

const ZIPFS: &str = env!("CARGO_BIN_EXE_zipfs");

fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists() && fusermount().is_some()
}

fn fusermount() -> Option<&'static str> {
    [
        "/usr/bin/fusermount3",
        "/bin/fusermount3",
        "/usr/bin/fusermount",
    ]
    .into_iter()
    .find(|p| Path::new(p).exists())
}

/// A directory to mount on, cleaned up however the test ends.
struct MountDir {
    path: PathBuf,
}

impl MountDir {
    fn new(name: &str) -> MountDir {
        let path = std::env::temp_dir().join(format!("zipfs-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir(&path);
        std::fs::create_dir_all(&path).unwrap();
        MountDir { path }
    }
}

impl Drop for MountDir {
    fn drop(&mut self) {
        if let Some(bin) = fusermount() {
            // Nothing is mounted where a test stopped before mounting, and
            // fusermount says so on its own stderr. Keep that out of the run.
            let _ = Command::new(bin).arg("-u").arg(&self.path).output();
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

#[test]
fn the_mount_serves_reads_as_soon_as_the_process_leaves() {
    if !fuse_available() {
        eprintln!("skipping: no /dev/fuse or fusermount3");
        return;
    }
    let temp = TempArchive::new(
        "cli-ready",
        &archive(&[Entry::file(
            "hello.txt",
            b"ready".to_vec(),
            CompressionMethod::STORE,
        )]),
    );
    let dir = MountDir::new("ready");

    // The option list is the one a fstab line arrives with, through
    // /sbin/mount.fuse: the archive and the mount point as arguments, and
    // everything else behind -o.
    let status = Command::new(ZIPFS)
        .arg(&temp.path)
        .arg(&dir.path)
        .arg("-o")
        .arg("ro,nosuid,nodev,noatime,subtype=zipfs,threads=2,attr-ttl=1")
        .status()
        .unwrap();
    assert!(status.success(), "mount failed: {status}");

    // No wait and no retry. The process left only after the mount answered the
    // kernel, so the file has to be there already.
    let body = std::fs::read(dir.path.join("hello.txt")).unwrap();
    assert_eq!(body, b"ready");
}

/// The binary under the name mount(8) gives a helper, which is what tells it
/// to read the short options the way mount(8) means them.
struct HelperLink {
    dir: PathBuf,
}

impl HelperLink {
    fn new(name: &str) -> HelperLink {
        let dir = std::env::temp_dir().join(format!("zipfs-helper-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(ZIPFS, dir.join("mount.fuse.zipfs")).unwrap();
        HelperLink { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("mount.fuse.zipfs")
    }
}

impl Drop for HelperLink {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_dry_run_through_the_helper_name_mounts_nothing() {
    let temp = TempArchive::new(
        "cli-fake",
        &archive(&[Entry::file(
            "hello.txt",
            b"ready".to_vec(),
            CompressionMethod::STORE,
        )]),
    );
    let dir = MountDir::new("fake");
    let helper = HelperLink::new("fake");

    // Exactly what `mount -f -n -s -v -t fuse.zipfs` hands its helper.
    let out = Command::new(helper.path())
        .arg(&temp.path)
        .arg(&dir.path)
        .args(["-s", "-f", "-n", "-v", "-o", "rw", "-t", "fuse.zipfs"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "dry run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // -f asks for everything but the mount, so nothing is there to read.
    std::fs::read(dir.path.join("hello.txt")).unwrap_err();
}

#[test]
fn the_helper_flags_do_not_stop_a_real_mount() {
    if !fuse_available() {
        eprintln!("skipping: no /dev/fuse or fusermount3");
        return;
    }
    let temp = TempArchive::new(
        "cli-helper",
        &archive(&[Entry::file(
            "hello.txt",
            b"ready".to_vec(),
            CompressionMethod::STORE,
        )]),
    );
    let dir = MountDir::new("helper");
    let helper = HelperLink::new("helper");

    let out = Command::new(helper.path())
        .arg(&temp.path)
        .arg(&dir.path)
        .args(["-s", "-n", "-o", "rw", "-t", "fuse.zipfs"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "mount failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(dir.path.join("hello.txt")).unwrap(), b"ready");
}

#[test]
fn asking_for_a_writable_mount_stops_the_mount() {
    let temp = TempArchive::new(
        "cli-rw",
        &archive(&[Entry::file(
            "a.txt",
            b"a".to_vec(),
            CompressionMethod::STORE,
        )]),
    );
    let dir = MountDir::new("rw");

    let out = Command::new(ZIPFS)
        .arg(&temp.path)
        .arg(&dir.path)
        .arg("-o")
        .arg("remount,rw")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let message = String::from_utf8_lossy(&out.stderr);
    assert!(
        message.contains("read-only"),
        "unexpected message: {message}"
    );
}
