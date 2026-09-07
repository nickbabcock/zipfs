#!/usr/bin/env bash
#
# Makes a Debian package from a zipfs binary that was built already.
#
# Usage: packaging/build-deb.sh <binary> <deb-architecture> <version> <outdir>
#
#   binary            the zipfs executable to install
#   deb-architecture  amd64 or arm64, as Debian names it
#   version           the package version, such as 0.1.1
#   outdir            where the .deb is written
#
# The binary given here is expected to be the static musl one. A static binary
# makes the package independent of the glibc on the machine, so one package per
# architecture is enough for every Debian and Ubuntu release that has fuse3.
set -euo pipefail
# Directories that the package makes belong to root and are read by everyone.
umask 022

if [ "$#" -ne 4 ]; then
    sed -n '3,12p' "$0" >&2
    exit 2
fi

binary=$1
arch=$2
version=$3
outdir=$4

root=$(cd -- "$(dirname -- "$0")/.." && pwd)
maintainer='Nick Babcock <nbabcock19@hotmail.com>'
homepage='https://github.com/nickbabcock/zipfs'

if [ ! -f "$binary" ]; then
    echo "build-deb: '$binary' does not exist" >&2
    exit 1
fi

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
# mktemp gives its directory to its owner alone, and that mode would reach the
# package as the mode of '/'.
chmod 0755 "$stage"

install -D -m 0755 "$binary" "$stage/usr/bin/zipfs"
# mount(8) starts /usr/sbin/mount.fuse.zipfs for the type fuse.zipfs, and the
# program takes its mount(8) meanings from the name it was started under. A
# relative link keeps the package independent of where it is unpacked.
mkdir -p "$stage/usr/sbin"
ln -s ../bin/zipfs "$stage/usr/sbin/mount.fuse.zipfs"

install -D -m 0644 "$root/man/zipfs.1" "$stage/usr/share/man/man1/zipfs.1"
gzip -9n "$stage/usr/share/man/man1/zipfs.1"

install -D -m 0644 "$root/CHANGELOG.md" "$stage/usr/share/doc/zipfs/changelog"
gzip -9n "$stage/usr/share/doc/zipfs/changelog"
install -D -m 0644 "$root/packaging/mnt-archive.mount" \
    "$stage/usr/share/doc/zipfs/examples/mnt-archive.mount"

# The copyright file is the license, in the format Debian reads.
{
    echo 'Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/'
    echo 'Upstream-Name: zipfs'
    echo "Source: $homepage"
    echo
    echo 'Files: *'
    echo 'Copyright: Nick Babcock'
    echo 'License: MIT'
    sed -e 's/^$/./' -e 's/^/ /' "$root/LICENSE.txt"
} > "$stage/usr/share/doc/zipfs/copyright"
chmod 0644 "$stage/usr/share/doc/zipfs/copyright"

# The size dpkg reports, in kibibytes, and without the control files.
installed_size=$(du -k -s --apparent-size "$stage" | cut -f1)

mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<CONTROL
Package: zipfs
Version: $version
Architecture: $arch
Maintainer: $maintainer
Installed-Size: $installed_size
Depends: fuse3
Section: utils
Priority: optional
Homepage: $homepage
Description: Read-only FUSE filesystem for zip archives
 zipfs mounts a zip archive as a read-only filesystem. The files in the
 archive are then read like any other files, with nothing written to disk:
 each read decompresses only the bytes it needs, and the kernel keeps the
 pages it has already seen.
 .
 Requests are served by several threads at once, which is what makes zipfs
 faster than the filesystems that serve one request at a time. It mounts
 archives of 100 GB and 100k entries in milliseconds.
 .
 The package installs a mount.fuse.zipfs helper, so an archive can also be
 mounted from mount(8), fstab or a systemd mount unit.
CONTROL

# md5sums lets dpkg and debsums see whether an installed file has changed.
# Symlinks have no contents to sum, so they are left out.
(cd "$stage" && find usr -type f -print0 | sort -z |
    xargs -0 md5sum > DEBIAN/md5sums)
chmod 0644 "$stage/DEBIAN/control" "$stage/DEBIAN/md5sums"

mkdir -p "$outdir"
deb="$outdir/zipfs_${version}_${arch}.deb"
# The owner of every file is root, whoever builds the package.
fakeroot dpkg-deb --root-owner-group --build "$stage" "$deb" >/dev/null
echo "$deb"
