# Changelog

## Unreleased

### Added

- Find a change to the archive while it is mounted. zipfs examines the
  archive after each read from it, before it uses the bytes. After a change,
  reads fail with `ESTALE`, and one error is logged. On a local filesystem, a
  rename or replacement of the archive is not a change, because the mount
  keeps the file that it opened. Over CIFS this is best effort, and over NFS a
  change can be found up to 60 seconds late.

### Changed

- Log to syslog after the process detaches. Before, these logs were lost.

## 0.1.2 - 2026-09-07

### Added

- Debian packages for `amd64` and `arm64`
- Builds for `aarch64`
- A `zipfs(1)` manual page.

## 0.1.1 - 2026-09-05

### Added

- Support `mount(8)` and `fstab` through a `mount.fuse.zipfs` helper.
- Parse `-o` options and accept standard kernel options and `mount(8)`.
- Add `--foreground`, mount-helper dry runs.
- Add the MIT license in `LICENSE.txt`.

### Changed

- Exit after the kernel accepts the mount, so scripts can use it when the
  command returns.
- Start directory reads at the requested offset.
- Speed up CRC32 verification with `crc32fast`.
