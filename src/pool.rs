//! Keeps a few positioned decoders alive per open file.
//!
//! A decoder is checked out only for the length of one read, and a read runs on
//! one FUSE worker thread, so at most one decoder per thread is ever in use no
//! matter how many files are open or how far ahead the kernel reads. The only
//! quantity that could grow without limit is the set of *idle* decoders, and a
//! counter is enough to bound that. Nothing here blocks.

use crate::codec::Codec;
use crate::decode::{CoreParts, EntryLocation, PositionedDecoder};
use crate::index::Method;
use rawzip::FileReader;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Locks a mutex, ignoring poisoning.
///
/// A panic on one worker must not take the whole mount down with it. Nothing
/// here keeps an invariant across a lock, so the worst a poisoned lock can hold
/// is a slightly stale list of idle decoders.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

const METHODS: usize = 3;

#[derive(Debug, Default)]
struct CoreStore {
    /// Codecs kept per method, since a deflate context cannot read zstd.
    codecs: [Vec<Codec>; METHODS],
    buffers: Vec<Box<[u8]>>,
    skips: Vec<Box<[u8]>>,
}

/// The mount-wide limit on decoder memory.
#[derive(Debug)]
pub struct DecoderBudget {
    /// Decoders currently sitting idle in some pool.
    retained: AtomicUsize,
    max_retained: usize,
    cores: Mutex<CoreStore>,
    max_cores: usize,
    source_buffer: usize,
    skip_buffer: usize,
    verify: bool,
}

impl DecoderBudget {
    #[must_use]
    pub fn new(config: &crate::Config) -> Arc<DecoderBudget> {
        Arc::new(DecoderBudget {
            retained: AtomicUsize::new(0),
            max_retained: config.max_retained_decoders,
            cores: Mutex::new(CoreStore::default()),
            max_cores: config.max_cores(),
            source_buffer: config.source_buffer,
            skip_buffer: crate::config::SKIP_BUFFER,
            verify: config.verify,
        })
    }

    pub fn verify(&self) -> bool {
        self.verify
    }

    /// How many idle decoders the mount is holding.
    pub fn retained(&self) -> usize {
        self.retained.load(Ordering::Relaxed)
    }

    /// Claims a slot for an idle decoder, if one is free.
    fn try_retain(&self) -> bool {
        self.retained
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < self.max_retained).then_some(n + 1)
            })
            .is_ok()
    }

    fn release_retain(&self) {
        self.retained.fetch_sub(1, Ordering::Relaxed);
    }

    /// Takes the expensive parts of a decoder, recycling them when possible.
    fn take_core(&self, method: Method) -> CoreParts {
        let mut store = lock(&self.cores);
        let codec = store.codecs[method as usize]
            .pop()
            .unwrap_or_else(|| Codec::new(method));
        let buf = store
            .buffers
            .pop()
            .unwrap_or_else(|| vec![0u8; self.source_buffer].into_boxed_slice());
        let skip = store
            .skips
            .pop()
            .unwrap_or_else(|| vec![0u8; self.skip_buffer].into_boxed_slice());
        CoreParts { codec, buf, skip }
    }

    /// Puts the expensive parts back, or drops them if enough are already kept.
    fn recycle_core(&self, parts: CoreParts) {
        let CoreParts { codec, buf, skip } = parts;
        let mut store = lock(&self.cores);
        let slot = codec.method() as usize;
        if store.codecs[slot].len() < self.max_cores {
            store.codecs[slot].push(codec);
        }
        if store.buffers.len() < self.max_cores {
            store.buffers.push(buf);
        }
        if store.skips.len() < self.max_cores {
            store.skips.push(skip);
        }
    }
}

/// The decoders belonging to one open file.
#[derive(Debug)]
pub struct EntryPool {
    /// The lock is held only for the list arithmetic; every byte of
    /// decompression happens after it has been released.
    idle: Mutex<Vec<PositionedDecoder>>,
    per_handle_cap: usize,
    budget: Arc<DecoderBudget>,
    reader: Arc<FileReader>,
    loc: EntryLocation,
}

impl EntryPool {
    pub fn new(
        budget: Arc<DecoderBudget>,
        reader: Arc<FileReader>,
        loc: EntryLocation,
        per_handle_cap: usize,
    ) -> EntryPool {
        EntryPool {
            idle: Mutex::new(Vec::new()),
            per_handle_cap,
            budget,
            reader,
            loc,
        }
    }

    /// Borrows a decoder that can reach `offset` without going backwards.
    ///
    /// Reading sequentially, or following the kernel's readahead, finds a
    /// decoder sitting at exactly the right offset. Reading backwards has to
    /// rewind one, which keeps its codec context and buffers but throws away
    /// its position.
    pub fn checkout(&self, offset: u64) -> Checkout<'_> {
        let picked = {
            let mut idle = lock(&self.idle);
            let exact = idle.iter().position(|d| d.pos() == offset && !d.finished());
            let usable = exact.or_else(|| {
                idle.iter()
                    .enumerate()
                    .filter(|(_, d)| d.pos() <= offset && !d.finished())
                    .max_by_key(|(_, d)| d.pos())
                    .map(|(i, _)| i)
            });
            if let Some(i) = usable {
                Some((idle.swap_remove(i), false))
            } else {
                // Everything idle is past the offset. Rewinding the one
                // that is furthest along gives up the least useful
                // position, and costs nothing but the reset itself.
                let furthest = idle
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, d)| d.pos())
                    .map(|(i, _)| i);
                furthest.map(|i| (idle.swap_remove(i), true))
            }
        };

        let decoder = if let Some((mut decoder, rewind)) = picked {
            self.budget.release_retain();
            if rewind {
                decoder.reinit(self.loc);
            }
            decoder
        } else {
            let parts = self.budget.take_core(self.loc.method);
            PositionedDecoder::new(
                Arc::clone(&self.reader),
                self.loc,
                parts,
                self.budget.verify(),
            )
        };

        Checkout {
            pool: self,
            decoder: Some(decoder),
        }
    }

    /// How many decoders this file is currently keeping.
    pub fn idle_len(&self) -> usize {
        lock(&self.idle).len()
    }

    fn give_back(&self, decoder: PositionedDecoder) {
        {
            let mut idle = lock(&self.idle);
            if idle.len() < self.per_handle_cap && self.budget.try_retain() {
                idle.push(decoder);
                return;
            }
        }
        self.budget.recycle_core(decoder.into_parts());
    }
}

impl Drop for EntryPool {
    fn drop(&mut self) {
        let idle = std::mem::take(&mut *lock(&self.idle));
        for decoder in idle {
            self.budget.release_retain();
            self.budget.recycle_core(decoder.into_parts());
        }
    }
}

/// A decoder borrowed from a pool, returned when it goes out of scope.
///
/// Returning through a guard means an error or a panic mid-read cannot lose a
/// decoder or leak its share of the budget.
#[derive(Debug)]
pub struct Checkout<'a> {
    pool: &'a EntryPool,
    decoder: Option<PositionedDecoder>,
}

impl Deref for Checkout<'_> {
    type Target = PositionedDecoder;

    fn deref(&self) -> &PositionedDecoder {
        self.decoder.as_ref().expect("decoder is checked out")
    }
}

impl DerefMut for Checkout<'_> {
    fn deref_mut(&mut self) -> &mut PositionedDecoder {
        self.decoder.as_mut().expect("decoder is checked out")
    }
}

impl Drop for Checkout<'_> {
    fn drop(&mut self) {
        if let Some(decoder) = self.decoder.take() {
            self.pool.give_back(decoder);
        }
    }
}
