//! The FUSE filesystem.
//!
//! Every operation takes `&self`, so the session can run as many event loops as
//! there are cores over one shared filesystem. Nothing on the read path takes a
//! lock that a decompression run could hold.

use crate::archive::Archive;
use crate::attr::file_attr;
use crate::config::{Config, MAX_IO};
use crate::decode::EntryLocation;
use crate::handle::{HandleTable, OpenFile, OpenKind};
use crate::index::{BuildStats, EntryMeta, Index, MAX_SYMLINK, Method, NodeKind, VERIFY_BAD};
use crate::pool::{DecoderBudget, EntryPool};
use flate2::Crc;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, OpenAccMode, OpenFlags, Request,
};
use fuser::{
    ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs,
};
use log::{debug, warn};
use rawzip::FileReader;
use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// The capabilities worth asking the kernel for.
const WANTED: InitFlags = InitFlags::FUSE_ASYNC_READ
    .union(InitFlags::FUSE_PARALLEL_DIROPS)
    .union(InitFlags::FUSE_DO_READDIRPLUS)
    .union(InitFlags::FUSE_READDIRPLUS_AUTO)
    .union(InitFlags::FUSE_MAX_PAGES)
    .union(InitFlags::FUSE_BIG_WRITES)
    .union(InitFlags::FUSE_CACHE_SYMLINKS);

thread_local! {
    /// A reply buffer per worker thread.
    ///
    /// `ReplyData` copies what it is given, so the same buffer serves every
    /// read this thread makes and no read has to allocate.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` with a thread-local buffer of at least `len` bytes.
fn with_scratch<R>(len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    SCRATCH.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < len {
            buf.resize(len, 0);
        }
        f(&mut buf[..len])
    })
}

/// A mounted zip archive.
#[derive(Debug)]
pub struct ZipFs {
    index: Arc<Index>,
    archive: Archive,
    reader: Arc<FileReader>,
    handles: HandleTable,
    budget: Arc<DecoderBudget>,
    config: Config,
    stats: BuildStats,
    /// The largest read the kernel will ask for, learned during `init`.
    max_io: AtomicU32,
}

impl ZipFs {
    /// Reads an archive's central directory and prepares it for mounting.
    ///
    /// # Errors
    ///
    /// Returns an error when the archive central directory cannot be read.
    pub fn new(archive: Archive, mut config: Config) -> crate::Result<ZipFs> {
        config.clamp();
        let (index, stats) = crate::index::build(&archive)?;
        Ok(ZipFs {
            reader: archive.reader(),
            budget: DecoderBudget::new(&config),
            index: Arc::new(index),
            archive,
            handles: HandleTable::new(),
            config,
            stats,
            max_io: AtomicU32::new(MAX_IO),
        })
    }

    #[must_use]
    pub fn index(&self) -> &Arc<Index> {
        &self.index
    }

    #[must_use]
    pub fn stats(&self) -> &BuildStats {
        &self.stats
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Checks that an entry can be read at all.
    fn readable(meta: &EntryMeta) -> Result<Method, Errno> {
        if meta.encrypted {
            return Err(Errno::from_i32(libc::EACCES));
        }
        let Some(method) = meta.method() else {
            debug!("compression method {} is not supported", meta.method_raw);
            return Err(Errno::from_i32(libc::EOPNOTSUPP));
        };
        Ok(method)
    }

    /// Where an entry's compressed bytes live.
    fn locate(&self, meta: &EntryMeta) -> Result<EntryLocation, Errno> {
        let method = Self::readable(meta)?;
        if method == Method::Store && meta.compressed_size != meta.uncompressed_size {
            return Err(Errno::from_i32(libc::EIO));
        }
        let data_start = self.archive.data_start(meta).map_err(|e| {
            warn!("could not locate entry data: {e}");
            Errno::from_i32(e.errno())
        })?;
        Ok(EntryLocation {
            data_start,
            compressed_size: meta.compressed_size,
            method,
        })
    }

    /// Decompresses a whole entry, refusing anything over `limit`.
    ///
    /// This is for symlink targets, which are tiny. Regular reads never use it.
    fn read_all(&self, meta: &EntryMeta, limit: usize) -> Result<Vec<u8>, Errno> {
        if meta.verify_state() == VERIFY_BAD {
            return Err(Errno::from_i32(libc::EIO));
        }
        let loc = self.locate(meta)?;
        let size = usize::try_from(meta.uncompressed_size).map_err(|error| {
            debug!("entry is too large to fit in memory: {error}");
            Errno::from_i32(libc::EIO)
        })?;
        if size > limit {
            return Err(Errno::from_i32(libc::EIO));
        }
        let mut out = vec![0u8; size];
        if size == 0 {
            if loc.method == Method::Store {
                self.verify_stored(meta, &out)?;
            } else {
                let pool =
                    EntryPool::new(Arc::clone(&self.budget), Arc::clone(&self.reader), loc, 1);
                let mut decoder = pool.checkout(0);
                decoder
                    .read_at(0, &mut out, meta)
                    .map_err(|e| Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)))?;
            }
            return Ok(out);
        }
        if loc.method == Method::Store {
            self.read_direct(&loc, 0, &mut out).map_err(|error| {
                debug!("could not read stored entry: {error}");
                Errno::from_i32(libc::EIO)
            })?;
            self.verify_stored(meta, &out)?;
            return Ok(out);
        }
        let pool = EntryPool::new(Arc::clone(&self.budget), Arc::clone(&self.reader), loc, 1);
        let mut decoder = pool.checkout(0);
        let n = decoder
            .read_at(0, &mut out, meta)
            .map_err(|e| Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)))?;
        if n != size {
            return Err(Errno::from_i32(libc::EIO));
        }
        Ok(out)
    }

    fn verify_stored(&self, meta: &EntryMeta, data: &[u8]) -> Result<(), Errno> {
        if !self.budget.verify() {
            return Ok(());
        }
        let mut crc = Crc::new();
        crc.update(data);
        let matches = crc.sum() == meta.crc32;
        meta.set_verify_state(if matches {
            crate::index::VERIFY_OK
        } else {
            VERIFY_BAD
        });
        if matches {
            Ok(())
        } else {
            Err(Errno::from_i32(libc::EIO))
        }
    }

    fn read_direct(&self, loc: &EntryLocation, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let length = buf.len() as u64;
        let end = offset.checked_add(length).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "read offset overflow")
        })?;
        if end > loc.compressed_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "read is outside the entry",
            ));
        }
        let absolute = loc.data_start.checked_add(offset).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "archive offset overflow")
        })?;
        absolute.checked_add(length).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "archive read overflow")
        })?;
        self.archive.read_at(buf, absolute)
    }

    /// Attributes for an inode that is known to exist.
    fn attr(&self, ino: u64) -> Option<FileAttr> {
        let node = self.index.node(ino)?;
        Some(file_attr(&self.index, ino, node, &self.config))
    }
}

impl fuser::Filesystem for ZipFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Asking for a capability the kernel does not have is an error, so only
        // ask for what it already offers.
        let available = config.capabilities();
        for flag in WANTED.iter() {
            if available.contains(flag) {
                let _ = config.add_capabilities(flag);
            }
        }
        // Even on a read-only filesystem this is what settles `max_pages`, and
        // so the size of an individual read. Left alone, reads stay at 128 KiB.
        let max_write = config.set_max_write(MAX_IO).unwrap_or_else(|v| v);
        let _ = config.set_max_readahead(MAX_IO);
        let _ = config.set_max_background(64);
        let _ = config.set_congestion_threshold(48);
        self.max_io.store(max_write.max(MAX_IO), Ordering::Relaxed);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.index.lookup(parent.into(), name.as_bytes()) {
            Some(ino) => match self.attr(ino) {
                Some(attr) => reply.entry(&self.config.attr_ttl, &attr, Generation(0)),
                None => reply.error(Errno::ENOENT),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr(ino.into()) {
            Some(attr) => reply.attr(&self.config.attr_ttl, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(node) = self.index.node(ino.into()) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if node.kind != NodeKind::Symlink {
            reply.error(Errno::from_i32(libc::EINVAL));
            return;
        }
        let Some(meta) = self.index.meta(node) else {
            reply.error(Errno::from_i32(libc::EIO));
            return;
        };
        // Targets are cached because a symlink is followed far more often than
        // it changes, which here is never.
        if let Some(target) = self.index.symlink(meta.symlink_slot) {
            reply.data(target);
            return;
        }
        match self.read_all(meta, MAX_SYMLINK) {
            Ok(target) => {
                let target = self
                    .index
                    .set_symlink(meta.symlink_slot, target.into_boxed_slice());
                match target {
                    Some(target) => reply.data(target),
                    None => reply.error(Errno::from_i32(libc::EIO)),
                }
            }
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let ino = u64::from(ino);
        let Some(node) = self.index.node(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if node.kind == NodeKind::Dir {
            reply.error(Errno::from_i32(libc::EISDIR));
            return;
        }
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::from_i32(libc::EROFS));
            return;
        }
        let Some(meta) = self.index.meta(node) else {
            reply.error(Errno::from_i32(libc::EIO));
            return;
        };
        // Resolving here rather than on the first read means a broken entry is
        // reported by `open`, and it costs one small read per entry for the
        // life of the mount rather than one per open.
        let loc = match self.locate(meta) {
            Ok(loc) => loc,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let file = OpenFile::new(
            Arc::clone(&self.index),
            Arc::clone(&self.reader),
            Arc::clone(&self.budget),
            ino,
            node.meta,
            loc,
            &self.config,
        );
        let fh = self.handles.insert(file);
        // The archive cannot change while it is mounted, so anything the page
        // cache already holds stays correct.
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(file) = self.handles.get(fh.into()) else {
            reply.error(Errno::from_i32(libc::EBADF));
            return;
        };
        if file.meta().verify_state() == VERIFY_BAD {
            reply.error(Errno::from_i32(libc::EIO));
            return;
        }
        let available = file.size.saturating_sub(offset);
        let want = u64::from(size).min(available);
        if want == 0 {
            reply.data(&[]);
            return;
        }
        let Ok(want) = usize::try_from(want) else {
            reply.error(Errno::from_i32(libc::EOVERFLOW));
            return;
        };

        with_scratch(want, |buf| {
            let result = match &file.kind {
                OpenKind::Empty => Ok(0),
                OpenKind::Store {
                    data_start,
                    compressed_size,
                    ..
                } => {
                    let loc = EntryLocation {
                        data_start: *data_start,
                        compressed_size: *compressed_size,
                        method: Method::Store,
                    };
                    match self.read_direct(&loc, offset, buf) {
                        // A stored entry has no decoder to carry its checksum,
                        // so the failure surfaces on the read that completes it.
                        Ok(()) if !file.observe_store(offset, buf) => Err(libc::EIO),
                        Ok(()) => Ok(buf.len()),
                        Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                    }
                }
                OpenKind::Coded(pool) => {
                    let mut decoder = pool.checkout(offset);
                    decoder
                        .read_at(offset, buf, file.meta())
                        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
                }
            };
            match result {
                Ok(n) => reply.data(&buf[..n]),
                Err(errno) => reply.error(Errno::from_i32(errno)),
            }
        });
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.remove(fh.into());
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.index.node(ino.into()) {
            Some(node) if node.kind == NodeKind::Dir => {
                reply.opened(FileHandle(0), FopenFlags::FOPEN_CACHE_DIR);
            }
            Some(_) => reply.error(Errno::from_i32(libc::ENOTDIR)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = u64::from(ino);
        let Some(node) = self.index.node(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if node.kind != NodeKind::Dir {
            reply.error(Errno::from_i32(libc::ENOTDIR));
            return;
        }
        // The tree never changes, so these cursors stay valid for as long as
        // the mount does.
        let parent = u64::from(node.parent);
        if offset < 1 && reply.add(INodeNo(ino), 1, FileType::Directory, ".") {
            reply.ok();
            return;
        }
        if offset < 2 && reply.add(INodeNo(parent), 2, FileType::Directory, "..") {
            reply.ok();
            return;
        }
        let first = u64::from(node.first_child);
        // Cursors are dense, so the offset says which child comes next. Start
        // there instead of walking the children before it.
        let start = usize::try_from(offset.saturating_sub(2)).unwrap_or(usize::MAX);
        let children = self.index.children(node);
        for (i, child) in children.get(start..).unwrap_or_default().iter().enumerate() {
            let i = start + i;
            let cursor = 3 + i as u64;
            let kind = match child.kind {
                NodeKind::Dir => FileType::Directory,
                NodeKind::File => FileType::RegularFile,
                NodeKind::Symlink => FileType::Symlink,
            };
            let name = OsStr::from_bytes(self.index.name(child));
            if reply.add(INodeNo(first + i as u64), cursor, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let ino = u64::from(ino);
        let Some(node) = self.index.node(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if node.kind != NodeKind::Dir {
            reply.error(Errno::from_i32(libc::ENOTDIR));
            return;
        }
        let ttl = self.config.attr_ttl;
        let parent = u64::from(node.parent);
        // Attributes come straight out of the index, so answering them here
        // costs nothing and saves the kernel a lookup per name.
        if offset < 1 {
            let attr = file_attr(&self.index, ino, node, &self.config);
            if reply.add(INodeNo(ino), 1, ".", &ttl, &attr, Generation(0)) {
                reply.ok();
                return;
            }
        }
        if offset < 2 {
            let pnode = self.index.node(parent).unwrap_or(node);
            let attr = file_attr(&self.index, parent, pnode, &self.config);
            if reply.add(INodeNo(parent), 2, "..", &ttl, &attr, Generation(0)) {
                reply.ok();
                return;
            }
        }
        let first = u64::from(node.first_child);
        let start = usize::try_from(offset.saturating_sub(2)).unwrap_or(usize::MAX);
        let children = self.index.children(node);
        for (i, child) in children.get(start..).unwrap_or_default().iter().enumerate() {
            let i = start + i;
            let cursor = 3 + i as u64;
            let child_ino = first + i as u64;
            let attr = file_attr(&self.index, child_ino, child, &self.config);
            let name = OsStr::from_bytes(self.index.name(child));
            if reply.add(INodeNo(child_ino), cursor, name, &ttl, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        match self.index.node(ino.into()) {
            // The mount is read-only and the kernel checks the mode bits, so
            // existing is all that is left to confirm.
            Some(_) => reply.ok(),
            None => reply.error(Errno::ENOENT),
        }
    }

    // The operations below are answered here rather than left to the default,
    // which logs every call it does not implement. Reporting ENOSYS once makes
    // the kernel stop asking for the rest of the mount.

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: fuser::ReplyXattr,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: fuser::ReplyXattr) {
        reply.error(Errno::ENOSYS);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Nothing is buffered on the way out, because nothing is written.
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let blocks = self.index.total_uncompressed().div_ceil(512);
        reply.statfs(blocks, 0, 0, self.index.len() as u64, 0, 512, 255, 0);
    }
}
