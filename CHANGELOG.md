# Changelog

## [0.1.1] - 2026-09-05

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
