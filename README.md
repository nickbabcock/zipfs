# zipfs

Mounts a zip archive as a read-only FUSE filesystem.

- Smaller and faster than fuse-zip especially on multi-threaded workloads
- Built for accessing large archives (100 GB and 100k entries).

```bash
zipfs archive.zip /mnt/archive
ls /mnt/archive
fusermount3 -u /mnt/archive
```

**Why build this**? Because agents know how to read files like the back of their hand.

## Notes

Requires fuse to be installed:

```bash
sudo apt install fuse3
```

The archive must not change while it is mounted. Its contents are treated as
fixed, which is what lets the kernel cache pages and metadata indefinitely.

- Supported compression methods: stored, deflate and zstd.
- Paths that are absolute, that contain `..`, or that hold a NUL byte are
  skipped, and the count is reported at mount.
- Where two entries claim one path, the first is kept. Where one path is a file
  and another makes it a directory, the directory is kept.
- Recommended to symlink zipfs as `/sbin/mount.fuse.zipfs` to work with `mount` and `fstab`:

  ```sh
  mount -t fuse.zipfs archive.zip /mnt/archive
  ```
