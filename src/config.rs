//! Runtime configuration, shared by the library and the binary.

use std::time::Duration;

/// The compressed bytes each decoder buffers before it asks the file for more.
pub const DEFAULT_SOURCE_BUFFER: usize = 512 * 1024;
/// The smallest source buffer that is accepted.
pub const MIN_SOURCE_BUFFER: usize = 4 * 1024;
/// The largest source buffer that is accepted.
pub const MAX_SOURCE_BUFFER: usize = 16 * 1024 * 1024;
/// Scratch space a decoder uses to discard bytes when it skips forward.
pub const SKIP_BUFFER: usize = 64 * 1024;
/// The read size negotiated with the kernel.
///
/// This is what `set_max_write` controls. Even on a read-only filesystem it
/// decides `max_pages`, which caps the size of an individual read request.
pub const MAX_IO: u32 = 1024 * 1024;

/// How the filesystem behaves for the lifetime of a mount.
#[derive(Debug, Clone)]
pub struct Config {
    /// Check every fully read entry against its CRC32.
    pub verify: bool,
    /// Bytes of compressed data each decoder buffers.
    pub source_buffer: usize,
    /// Idle decoders one open file may keep.
    pub decoders_per_file: usize,
    /// Idle decoders the whole mount may keep.
    pub max_retained_decoders: usize,
    /// FUSE event loop threads.
    pub threads: usize,
    /// Owner reported for every inode.
    pub uid: u32,
    /// Group reported for every inode.
    pub gid: u32,
    /// Permissions for files whose entry does not record any.
    pub file_mode: u16,
    /// Permissions for directories whose entry does not record any.
    pub dir_mode: u16,
    /// How long the kernel may cache attributes and directory entries.
    ///
    /// The archive cannot change while it is mounted, so this is very long.
    pub attr_ttl: Duration,
    /// Let other users see the mount.
    pub allow_other: bool,
    /// Unmount if the process dies.
    ///
    /// FUSE refuses this unless the mount is visible to more than its owner,
    /// so it requires `allow_other`.
    pub auto_unmount: bool,
}

impl Default for Config {
    fn default() -> Self {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .map_or(4, |n| n.min(8));
        Config {
            verify: true,
            source_buffer: DEFAULT_SOURCE_BUFFER,
            decoders_per_file: 4,
            max_retained_decoders: 64,
            threads,
            // SAFETY: `geteuid` reads the effective user ID and has no
            // preconditions.
            uid: unsafe { libc::geteuid() },
            // SAFETY: `getegid` reads the effective group ID and has no
            // preconditions.
            gid: unsafe { libc::getegid() },
            file_mode: 0o644,
            dir_mode: 0o755,
            attr_ttl: Duration::from_secs(31_536_000),
            allow_other: false,
            auto_unmount: false,
        }
    }
}

impl Config {
    /// Brings every field into its permitted range.
    pub fn clamp(&mut self) {
        self.source_buffer = self
            .source_buffer
            .clamp(MIN_SOURCE_BUFFER, MAX_SOURCE_BUFFER);
        self.threads = self.threads.max(1);
        // The mount-wide cap is the one that bounds memory, so a per-file cap
        // above it would have no effect anyway.
        self.max_retained_decoders = self.max_retained_decoders.max(1);
        self.decoders_per_file = self.decoders_per_file.clamp(1, self.max_retained_decoders);
        self.file_mode &= 0o7777;
        self.dir_mode &= 0o7777;
    }

    /// Recyclable decoder cores the mount keeps around.
    ///
    /// A core holds the buffers and, for zstd, the decompression context. Those
    /// are the expensive parts and they do not depend on which entry a decoder
    /// is reading, so they outlive the decoders themselves.
    #[must_use]
    pub fn max_cores(&self) -> usize {
        self.threads * 2
    }
}
