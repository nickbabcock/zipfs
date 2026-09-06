//! Open files.

use crate::decode::EntryLocation;
use crate::hash::IdentityBuildHasher;
use crate::index::{EntryMeta, Index, VERIFY_BAD, VERIFY_OK};
use crate::pool::{DecoderBudget, EntryPool};
use crc32fast::Hasher as Crc;
use rawzip::FileReader;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Follows a stored entry's checksum while reads arrive in order.
///
/// A stored entry has no decoder to carry the checksum, so it is tracked here
/// instead. With several worker threads the kernel often delivers readahead out
/// of order, in which case this simply gives up: a stored read is one `pread`,
/// and reading the entry a second time to check it would double the I/O.
#[derive(Debug)]
pub struct StoreVerify {
    crc: Crc,
    pos: u64,
    disabled: bool,
}

impl StoreVerify {
    fn new() -> StoreVerify {
        StoreVerify {
            crc: Crc::new(),
            pos: 0,
            disabled: false,
        }
    }

    /// Feeds in a block that was just read.
    ///
    /// Returns false once the entry is known not to match its checksum.
    fn observe(&mut self, offset: u64, data: &[u8], meta: &EntryMeta) -> bool {
        if self.disabled || offset != self.pos {
            self.disabled = true;
            return true;
        }
        self.crc.update(data);
        self.pos += data.len() as u64;
        if self.pos < meta.uncompressed_size {
            return true;
        }
        self.disabled = true;
        let matches = self.crc.clone().finalize() == meta.crc32;
        meta.set_verify_state(if matches { VERIFY_OK } else { VERIFY_BAD });
        matches
    }
}

/// How an open file gets at its bytes.
#[derive(Debug)]
pub enum OpenKind {
    /// Nothing to read.
    Empty,
    /// Stored entries are read straight out of the file at an offset, with no
    /// decoder and no shared state.
    Store {
        data_start: u64,
        compressed_size: u64,
        verify: Option<Mutex<StoreVerify>>,
    },
    /// Compressed entries need a decoder positioned inside the stream.
    Coded(EntryPool),
}

/// One open file.
#[derive(Debug)]
pub struct OpenFile {
    pub ino: u64,
    pub meta_idx: u32,
    pub size: u64,
    pub kind: OpenKind,
    index: Arc<Index>,
}

impl OpenFile {
    pub fn new(
        index: Arc<Index>,
        reader: Arc<FileReader>,
        budget: Arc<DecoderBudget>,
        ino: u64,
        meta_idx: u32,
        loc: EntryLocation,
        config: &crate::Config,
    ) -> OpenFile {
        let size = index.metas[meta_idx as usize].uncompressed_size;
        let verify = budget.verify();
        let kind = if size == 0 {
            OpenKind::Empty
        } else if loc.method == crate::index::Method::Store {
            OpenKind::Store {
                data_start: loc.data_start,
                compressed_size: loc.compressed_size,
                verify: verify.then(|| Mutex::new(StoreVerify::new())),
            }
        } else {
            OpenKind::Coded(EntryPool::new(
                budget,
                reader,
                loc,
                config.decoders_per_file,
            ))
        };
        OpenFile {
            ino,
            meta_idx,
            size,
            kind,
            index,
        }
    }

    /// The entry this file was opened on.
    pub fn meta(&self) -> &EntryMeta {
        &self.index.metas[self.meta_idx as usize]
    }

    /// Records a block of a stored entry against its checksum.
    ///
    /// Returns false once the entry is known not to match.
    pub fn observe_store(&self, offset: u64, data: &[u8]) -> bool {
        let OpenKind::Store {
            verify: Some(verify),
            ..
        } = &self.kind
        else {
            return true;
        };
        lock(verify).observe(offset, data, self.meta())
    }
}

/// The open files of a mount.
#[derive(Debug)]
pub struct HandleTable {
    /// File handles are a dense counter, so they hash to themselves.
    map: RwLock<HashMap<u64, Arc<OpenFile>, IdentityBuildHasher>>,
    next: AtomicU64,
}

impl Default for HandleTable {
    fn default() -> Self {
        HandleTable::new()
    }
}

impl HandleTable {
    #[must_use]
    pub fn new() -> HandleTable {
        HandleTable {
            map: RwLock::new(HashMap::default()),
            // Zero is what the kernel sends for a handle that was never set.
            next: AtomicU64::new(1),
        }
    }

    pub fn insert(&self, file: OpenFile) -> u64 {
        let fh = self.next.fetch_add(1, Ordering::Relaxed);
        self.write().insert(fh, Arc::new(file));
        fh
    }

    /// Looks up an open file, holding the lock only long enough to clone the
    /// reference. A read in flight therefore keeps the file alive even if
    /// another thread closes it.
    pub fn get(&self, fh: u64) -> Option<Arc<OpenFile>> {
        self.read().get(&fh).cloned()
    }

    pub fn remove(&self, fh: u64) -> Option<Arc<OpenFile>> {
        self.write().remove(&fh)
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, HashMap<u64, Arc<OpenFile>, IdentityBuildHasher>> {
        self.map.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<u64, Arc<OpenFile>, IdentityBuildHasher>> {
        self.map.write().unwrap_or_else(PoisonError::into_inner)
    }
}
