# zipfs

Mounts a zip archive as a read-only FUSE filesystem.

```console
zipfs archive.zip /mnt/archive
ls /mnt/archive
fusermount3 -u /mnt/archive
```

Supported compression methods: stored, deflate and zstd.

- Smaller and faster than fuse-zip especially on multi-threaded workloads
- Built for accessing large archives (100 GB and 100k entries).

## Notes

The archive must not change while it is mounted. Its contents are treated as
fixed, which is what lets the kernel cache pages and metadata indefinitely.

- Paths that are absolute, that contain `..`, or that hold a NUL byte are
  skipped, and the count is reported at mount.
- Where two entries claim one path, the first is kept. Where one path is a file
  and another makes it a directory, the directory is kept.
