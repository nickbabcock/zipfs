//! Reads a mounted archive through the kernel.
//!
//! These tests need `/dev/fuse` and `fusermount3`. Where either is missing they
//! report that and pass, so the suite still runs in a container without FUSE.

mod common;

use common::{Entry, TempArchive, archive, compressible, pseudo_random};
use rawzip::CompressionMethod;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(feature = "zstd")]
use std::sync::Arc;
use zipfs::{Archive, Config, ZipFs};

const STORE: CompressionMethod = CompressionMethod::STORE;
const DEFLATE: CompressionMethod = CompressionMethod::DEFLATE;
#[cfg(feature = "zstd")]
const ZSTD: CompressionMethod = CompressionMethod::ZSTD;

/// Whether this machine can mount a FUSE filesystem at all.
fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists()
        && [
            "/usr/bin/fusermount3",
            "/bin/fusermount3",
            "/usr/bin/fusermount",
        ]
        .iter()
        .any(|p| Path::new(p).exists())
}

/// A mounted archive that unmounts when it goes out of scope.
struct Mount {
    dir: PathBuf,
    session: Option<fuser::BackgroundSession>,
    _temp: TempArchive,
}

impl Mount {
    fn new(name: &str, entries: &[Entry], config: &Config) -> Mount {
        let temp = TempArchive::new(name, &archive(entries));
        Self::from_temp(name, temp, config)
    }

    fn from_bytes(name: &str, bytes: &[u8], config: &Config) -> Mount {
        Self::from_temp(name, TempArchive::new(name, bytes), config)
    }

    fn from_temp(name: &str, temp: TempArchive, config: &Config) -> Mount {
        let dir = std::env::temp_dir().join(format!(
            "zipfs-mnt-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir(&dir);
        fs::create_dir_all(&dir).unwrap();

        let archive = Archive::open(&temp.path).unwrap();
        let fs_impl = ZipFs::new(archive, config.clone()).unwrap();
        let mut session_config = fuser::Config::default();
        session_config.mount_options = vec![
            fuser::MountOption::RO,
            fuser::MountOption::NoAtime,
            fuser::MountOption::FSName("zipfs-test".to_string()),
            fuser::MountOption::DefaultPermissions,
        ];
        session_config.n_threads = Some(config.threads);
        session_config.clone_fd = true;
        let session = fuser::Session::new(fs_impl, &dir, &session_config)
            .unwrap()
            .spawn()
            .unwrap();
        Mount {
            dir,
            session: Some(session),
            _temp: temp,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = session.umount_and_join();
        }
        let _ = fs::remove_dir(&self.dir);
    }
}

/// Skips the body of a test when FUSE is not usable here.
macro_rules! needs_fuse {
    () => {
        if !fuse_available() {
            eprintln!("skipping: /dev/fuse or fusermount3 is not available");
            return;
        }
    };
}

fn sample_entries() -> Vec<Entry> {
    let entries = vec![
        Entry::file("stored.bin", pseudo_random(300_000, 3), STORE),
        Entry::file("deflated.txt", compressible(400_000), DEFLATE),
        Entry::file("empty", Vec::new(), STORE),
        Entry::file("dir/nested/leaf.txt", "leaf", STORE),
        Entry::file("dir/beta", "b", STORE),
        Entry::file("dir/alpha", "a", STORE),
        Entry::symlink("link", "deflated.txt"),
        Entry::file("readonly", "r", STORE).mode(0o100_400),
    ];
    #[cfg(feature = "zstd")]
    return entries
        .into_iter()
        .chain([Entry::file("zstd.txt", compressible(400_000), ZSTD)])
        .collect();
    #[cfg(not(feature = "zstd"))]
    entries
}

#[test]
fn a_directory_lists_its_entries_in_order() {
    needs_fuse!();
    let mount = Mount::new("listing", &sample_entries(), &Config::default());

    let mut names: Vec<String> = fs::read_dir(&mount.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let expected = {
        #[cfg(feature = "zstd")]
        {
            vec![
                "deflated.txt",
                "dir",
                "empty",
                "link",
                "readonly",
                "stored.bin",
                "zstd.txt",
            ]
        }
        #[cfg(not(feature = "zstd"))]
        {
            vec![
                "deflated.txt",
                "dir",
                "empty",
                "link",
                "readonly",
                "stored.bin",
            ]
        }
    };
    assert_eq!(names, expected);

    let mut nested: Vec<String> = fs::read_dir(mount.path("dir"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    nested.sort();
    assert_eq!(nested, ["alpha", "beta", "nested"]);
}

#[test]
fn metadata_matches_the_archive() {
    needs_fuse!();
    let mount = Mount::new("metadata", &sample_entries(), &Config::default());

    let meta = fs::metadata(mount.path("deflated.txt")).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.len(), 400_000);

    let dir = fs::metadata(mount.path("dir")).unwrap();
    assert!(dir.is_dir());

    let readonly = fs::metadata(mount.path("readonly")).unwrap();
    assert_eq!(readonly.mode() & 0o777, 0o400);

    let link = fs::symlink_metadata(mount.path("link")).unwrap();
    assert!(link.file_type().is_symlink());
    assert_eq!(
        fs::read_link(mount.path("link")).unwrap(),
        Path::new("deflated.txt")
    );
}

#[test]
fn a_stored_symlink_crc_failure_is_reported() {
    needs_fuse!();
    let mut bytes = archive(&[Entry::symlink("link", "target")]);
    patch_central_crc(&mut bytes, b"link", u32::MAX);
    let mount = Mount::from_bytes("corrupt-symlink", &bytes, &Config::default());

    fs::read_link(mount.path("link")).unwrap_err();
}

#[test]
fn every_method_reads_back_whole() {
    needs_fuse!();
    let entries = sample_entries();
    let mount = Mount::new("whole", &entries, &Config::default());

    for entry in &entries {
        if entry.path.ends_with('/') || entry.mode == Some(0o120_777) {
            continue;
        }
        let got = fs::read(mount.path(entry.path)).unwrap();
        assert_eq!(got, entry.body, "{} did not read back", entry.path);
    }
}

#[test]
fn reads_at_an_offset_and_past_the_end_behave() {
    needs_fuse!();
    let body = pseudo_random(500_000, 23);
    let entries = vec![
        Entry::file("stored", body.clone(), STORE),
        Entry::file("deflated", body.clone(), DEFLATE),
    ];
    let mount = Mount::new("offsets", &entries, &Config::default());

    for name in ["stored", "deflated"] {
        let mut file = fs::File::open(mount.path(name)).unwrap();
        let mut buf = vec![0u8; 4096];

        for offset in [0u64, 1, 65_536, 499_000] {
            file.seek(SeekFrom::Start(offset)).unwrap();
            let n = file.read(&mut buf).unwrap();
            let start = usize::try_from(offset).expect("test offset fits in usize");
            assert_eq!(buf[..n], body[start..start + n], "{name} at {offset}");
        }

        // Backwards, which makes the pool rewind a decoder.
        file.seek(SeekFrom::Start(1024)).unwrap();
        let n = file.read(&mut buf).unwrap();
        assert_eq!(buf[..n], body[1024..1024 + n]);

        // Past the end reads nothing and is not an error.
        file.seek(SeekFrom::Start(body.len() as u64 + 10)).unwrap();
        assert_eq!(file.read(&mut buf).unwrap(), 0);
    }
}

#[cfg(feature = "zstd")]
#[test]
fn many_threads_read_many_files_at_once() {
    needs_fuse!();
    let bodies: Vec<Vec<u8>> = (0..4).map(|i| pseudo_random(400_000, 31 + i)).collect();
    let entries = vec![
        Entry::file("a", bodies[0].clone(), STORE),
        Entry::file("b", bodies[1].clone(), DEFLATE),
        Entry::file("c", bodies[2].clone(), ZSTD),
        Entry::file("d", bodies[3].clone(), DEFLATE),
    ];
    let mount = Mount::new("parallel", &entries, &Config::default());
    let bodies = Arc::new(bodies);

    std::thread::scope(|scope| {
        for thread in 0..8usize {
            let bodies = Arc::clone(&bodies);
            let dir = mount.dir.clone();
            scope.spawn(move || {
                let names = ["a", "b", "c", "d"];
                let mut buf = vec![0u8; 5_000];
                for step in 0..12usize {
                    let which = (thread + step) % 4;
                    let mut file = fs::File::open(dir.join(names[which])).unwrap();
                    let offset = ((thread * 7919 + step * 40_009) % 390_000) as u64;
                    file.seek(SeekFrom::Start(offset)).unwrap();
                    let mut got = 0;
                    while got < buf.len() {
                        let n = file.read(&mut buf[got..]).unwrap();
                        if n == 0 {
                            break;
                        }
                        got += n;
                    }
                    let start = usize::try_from(offset).expect("test offset fits in usize");
                    assert_eq!(
                        buf[..got],
                        bodies[which][start..start + got],
                        "{} at {offset}",
                        names[which]
                    );
                }
            });
        }
    });
}

#[test]
fn a_corrupt_entry_fails_to_read_unless_checking_is_turned_off() {
    needs_fuse!();
    let body = compressible(80_000);
    let mut bytes = archive(&[Entry::file("data", body.clone(), DEFLATE)]);
    let target = bytes.len() / 2;
    bytes[target] ^= 0x40;

    for verify in [true, false] {
        let temp = TempArchive::new("corrupt-mount", &bytes);
        let dir =
            std::env::temp_dir().join(format!("zipfs-mnt-corrupt-{}-{verify}", std::process::id()));
        let _ = fs::remove_dir(&dir);
        fs::create_dir_all(&dir).unwrap();

        let config = Config {
            verify,
            ..Config::default()
        };
        let archive = Archive::open(&temp.path).unwrap();
        let mut session_config = fuser::Config::default();
        session_config.mount_options = vec![
            fuser::MountOption::RO,
            fuser::MountOption::DefaultPermissions,
        ];
        let session =
            fuser::Session::new(ZipFs::new(archive, config).unwrap(), &dir, &session_config)
                .unwrap()
                .spawn()
                .unwrap();

        let result = fs::read(dir.join("data"));
        if verify {
            assert!(result.is_err(), "the checksum failure was not reported");
        } else {
            assert!(result.is_ok(), "reading without a check should succeed");
        }

        let _ = session.umount_and_join();
        let _ = fs::remove_dir(&dir);
    }
}

#[test]
fn writes_are_refused() {
    needs_fuse!();
    let mount = Mount::new("readonly", &sample_entries(), &Config::default());
    assert!(fs::write(mount.path("new.txt"), "nope").is_err());
    assert!(fs::remove_file(mount.path("empty")).is_err());
}

fn patch_central_crc(bytes: &mut [u8], name: &[u8], crc: u32) {
    const CENTRAL_HEADER: &[u8; 4] = b"PK\x01\x02";
    const CENTRAL_FIXED: usize = 46;
    const CRC: usize = 16;
    const NAME_LENGTH: usize = 28;
    const NAME: usize = 46;

    for offset in 0..=bytes.len().saturating_sub(CENTRAL_FIXED) {
        if &bytes[offset..offset + 4] != CENTRAL_HEADER {
            continue;
        }
        let name_len = u16::from_le_bytes(
            bytes[offset + NAME_LENGTH..offset + NAME_LENGTH + 2]
                .try_into()
                .unwrap(),
        ) as usize;
        if name_len == name.len() && bytes[offset + NAME..offset + NAME + name_len] == *name {
            bytes[offset + CRC..offset + CRC + 4].copy_from_slice(&crc.to_le_bytes());
            return;
        }
    }
    panic!("central directory entry not found");
}
