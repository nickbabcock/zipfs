# zipfs

Mounts a zip archive as a read-only FUSE filesystem.

- **Scales with cores.** zipfs is the only zip FUSE filesystem that serves reads in parallel: 5x faster than mount-zip and fuse-zip.
- **Re-reads at page-cache speed.** Decoded pages stay in the page cache across opens, unlocking a 100x faster second read.
- **Instant mount, no scratch space.** A 10 GB archive with 10k entries mounts in 10 ms with sub 5 MB idle RSS and without decoding anything to disk or RAM.
- **Detects a changed archive.** Reads fail with `ESTALE` and never return stale data.
- **No runtime dependencies.** zipfs is one static binary with no libfuse, libzip or Boost.

```bash
zipfs archive.zip /mnt/archive
ls /mnt/archive
fusermount3 -u /mnt/archive
```

**Why build this**? Because agents know how to read files like the back of their hand.

## Install

### Debian and Ubuntu

Download and install the `.deb` for `amd64` or `arm64`:

```bash
sudo apt install ./zipfs_0.1.1_amd64.deb
```

### Direct download

Download the `zipfs` executable for your architecture and install it:

```bash
sudo install -m 0755 zipfs /usr/local/bin/zipfs
```

Install `fuse3` if it is not already installed:

```bash
sudo apt install fuse3
```

## Notes

The archive must not change while it is mounted. Its contents are treated as
fixed, which is what lets the kernel cache pages and metadata indefinitely.

If the archive changes, zipfs logs an error and reads fail with `ESTALE`.
Mount the archive again to read the new contents. zipfs examines the archive
after each read from it, before it uses the bytes. It finds a change when the
size or the modification time is different. A change to the permissions or
other metadata only is not a change. Reads that the page cache serves do not
reach zipfs and cost nothing. Without `--foreground`, zipfs logs to syslog, so
`journalctl -t zipfs` shows the error.

On a local filesystem, zipfs keeps the file that it mounted. If you rename,
remove or replace the archive, the mount continues to show the old contents.
A write that keeps the size and the recorded modification time is not found.
This can occur when the filesystem records time in large steps, for example
2 seconds on FAT.

Over CIFS, the client can open the path again after a reconnect. So zipfs also
examines the path after each read. This protection is best effort only.

Over NFS, a change made on another host is found only once the attribute
cache expires, up to 60 seconds by default. Mount with `actimeo=0` to find it
on the next read.

- Supported compression methods: stored, deflate and zstd.
- Paths that are absolute, that contain `..`, or that hold a NUL byte are
  skipped, and the count is reported at mount.
- Where two entries claim one path, the first is kept. Where one path is a file
  and another makes it a directory, the directory is kept.
- The `.deb` installs `/usr/sbin/mount.fuse.zipfs`, which is what lets `mount`
  and `fstab` reach zipfs. After a manual install, make that link by hand.
  Then:

  ```sh
  mount -t fuse.zipfs archive.zip /mnt/archive
  ```

  An fstab line reads:

  ```
  /srv/data.zip  /mnt/data  fuse.zipfs  ro,noauto,allow_other  0 0
  ```

## systemd

Copy [`packaging/mnt-archive.mount`](packaging/mnt-archive.mount) to
`/etc/systemd/system/`, then change `What` and `Where`. The filename must match
the mount point, so `/mnt/archive` becomes `mnt-archive.mount`:

```bash
sudo cp packaging/mnt-archive.mount /etc/systemd/system/mnt-archive.mount
sudo systemctl daemon-reload
sudo systemctl start mnt-archive.mount
```

See `man zipfs` for the available options.
