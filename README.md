# zipfs

Mounts a zip archive as a read-only FUSE filesystem.

- Natively multi-threaded. With 12 threads, zipfs is 5x faster than mount-zip and fuse-zip, which serve one request at a time.
- Repeat reads come from the kernel page cache without decoding a second time, yielding 100x improvement over `mount-zip` and `fuse-zip`.
- Built for large archives (100 GB and 100k entries). It mounts in milliseconds
  and never writes a decompressed copy to disk.

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
