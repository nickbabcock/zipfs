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

Each release has a `.deb` for `amd64` and `arm64`:

```bash
sudo apt install ./zipfs_0.1.1_amd64.deb
```

The package installs `/usr/bin/zipfs`, the mount helper
`/usr/sbin/mount.fuse.zipfs`, the `zipfs(1)` manual page, and an example
systemd mount unit at `/usr/share/doc/zipfs/examples/mnt-archive.mount`. It
depends on `fuse3` and on nothing else: the binary in it is static, so one
package per architecture serves every release that has fuse3.

### Tarball

The GNU and musl tarballs stay available for a manual install, in `x86_64` and
`aarch64`. Each holds the binary, the manual page and the example unit:

```bash
tar xzf zipfs-v0.1.1-linux-x86_64-musl.tar.gz
cd zipfs-v0.1.1-linux-x86_64-musl
sudo install -m 0755 zipfs /usr/local/bin/zipfs
sudo ln -s /usr/local/bin/zipfs /usr/local/sbin/mount.fuse.zipfs
sudo install -m 0644 zipfs.1 /usr/local/share/man/man1/zipfs.1
```

fuse3 is needed either way:

```bash
sudo apt install fuse3
```

### Verify a download

Every artifact has a SHA-256 checksum and a [Sigstore][] signature, made by the
release workflow with no key that this repository keeps:

```bash
sha256sum -c zipfs_0.1.1_amd64.deb.sha256

cosign verify-blob \
  --bundle zipfs_0.1.1_amd64.deb.sigstore.json \
  --certificate-identity-regexp '^https://github.com/nickbabcock/zipfs/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  zipfs_0.1.1_amd64.deb
```

[Sigstore]: https://www.sigstore.dev/

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

A mount unit takes the same options. The name of the file must agree with the
mount point, so `/mnt/archive` becomes `mnt-archive.mount`:

```ini
[Unit]
Description=Zip archive at /mnt/archive

[Mount]
What=/srv/archive.zip
Where=/mnt/archive
Type=fuse.zipfs
Options=ro,nosuid,nodev,allow_other,threads=4

[Install]
WantedBy=multi-user.target
```

```bash
sudo cp mnt-archive.mount /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl start mnt-archive.mount
```

The full example is in [`packaging/mnt-archive.mount`](packaging/mnt-archive.mount),
and `man zipfs` says what each option does.
