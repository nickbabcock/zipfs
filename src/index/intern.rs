//! The build-time map from `(parent, name component)` to a node.
//!
//! This is an open-addressing table rather than a `HashMap` because the keys
//! live in the name blob. A `HashMap` would need each component in its own
//! allocation, which is half a million allocations for a 100k entry archive.

use crate::hash::FxHasher;
use std::hash::Hasher;

/// A node as it exists while the tree is being discovered.
#[derive(Clone, Copy)]
pub struct TmpNode {
    pub name_off: u32,
    pub name_len: u16,
    pub kind: crate::index::NodeKind,
    pub flags: u8,
    pub parent: u32,
    pub meta: u32,
}

pub struct Interner {
    /// Each slot holds a node index plus one; zero means empty.
    slots: Box<[u32]>,
    mask: usize,
    len: usize,
}

#[inline]
fn hash_key(parent: u32, name: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write_u32(parent);
    h.write(name);
    h.finish()
}

#[inline]
#[allow(
    clippy::cast_possible_truncation,
    reason = "The table size is a power of two, so only low bits select a bucket."
)]
fn bucket(hash: u64, mask: usize) -> usize {
    hash as usize & mask
}

impl Interner {
    pub fn with_capacity(cap: usize) -> Interner {
        // Keep the load factor under one half so probe runs stay short.
        let size = (cap.max(16) * 2).next_power_of_two();
        Interner {
            slots: vec![0u32; size].into_boxed_slice(),
            mask: size - 1,
            len: 0,
        }
    }

    /// Finds the node with this parent and name.
    pub fn find(&self, nodes: &[TmpNode], names: &[u8], parent: u32, name: &[u8]) -> Option<u32> {
        let mut idx = bucket(hash_key(parent, name), self.mask);
        loop {
            let slot = self.slots[idx];
            if slot == 0 {
                return None;
            }
            let id = slot - 1;
            let n = &nodes[id as usize];
            if n.parent == parent && node_name(n, names) == name {
                return Some(id);
            }
            idx = (idx + 1) & self.mask;
        }
    }

    /// Records a node that `find` did not turn up.
    pub fn insert(&mut self, nodes: &[TmpNode], names: &[u8], id: u32) {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow(nodes, names);
        }
        self.place(nodes, names, id);
        self.len += 1;
    }

    fn place(&mut self, nodes: &[TmpNode], names: &[u8], id: u32) {
        let n = &nodes[id as usize];
        let mut idx = bucket(hash_key(n.parent, node_name(n, names)), self.mask);
        while self.slots[idx] != 0 {
            idx = (idx + 1) & self.mask;
        }
        self.slots[idx] = id + 1;
    }

    fn grow(&mut self, nodes: &[TmpNode], names: &[u8]) {
        let old = std::mem::replace(&mut self.slots, Box::new([]));
        let size = old.len() * 2;
        self.slots = vec![0u32; size].into_boxed_slice();
        self.mask = size - 1;
        for slot in &old {
            if *slot != 0 {
                self.place(nodes, names, slot - 1);
            }
        }
    }
}

#[inline]
fn node_name<'a>(n: &TmpNode, names: &'a [u8]) -> &'a [u8] {
    let start = n.name_off as usize;
    &names[start..start + n.name_len as usize]
}
