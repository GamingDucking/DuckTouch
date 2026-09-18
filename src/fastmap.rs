/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! A fast, non-cryptographic hasher for the emulator's internal hot maps.
//!
//! `std`'s default `HashMap` hasher is SipHash-1-3, designed to resist
//! deliberate hash-collision attacks. That resistance is irrelevant for maps
//! whose keys are our own guest pointers / selector handles (small integers
//! effectively), while its cost is very real: the Objective-C runtime does
//! several `HashMap` lookups **per guest message send** (object map, class
//! initialised set, method tables), and memory management does more per
//! retain/release/autorelease. On mobile CPUs this is a measurable fraction
//! of the per-message overhead.
//!
//! This is a compact FxHash-style ("multiplicative rotate-xor") hasher in
//! the spirit of `rustc-hash`, specialised for the integer-sized keys used
//! by the runtime maps. Do not use it for attacker-controlled key spaces;
//! for those keep `std`'s default hasher.

use std::hash::{BuildHasherDefault, Hasher};

/// FxHash-style hasher: low setup cost, excellent small-integer behaviour.
#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

/// Multiplicative mixing constant, borrowed from `rustc-hash` (originally
/// from fmix-style finalisers).
const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add_to_hash(&mut self, value: u64) {
        self.hash = (self.hash.rotate_left(5) ^ value).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add_to_hash(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        let remainder = chunks.remainder();
        if !remainder.is_empty() {
            let mut tail: u64 = 0;
            for (i, &b) in remainder.iter().enumerate() {
                tail |= (b as u64) << (i * 8);
            }
            self.add_to_hash(tail);
        }
    }
    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.add_to_hash(value as u64);
    }
    #[inline]
    fn write_u16(&mut self, value: u16) {
        self.add_to_hash(value as u64);
    }
    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.add_to_hash(value as u64);
    }
    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add_to_hash(value);
    }
    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add_to_hash(value as u64);
    }
    #[inline]
    fn write_u128(&mut self, value: u128) {
        self.add_to_hash(value as u64);
        self.add_to_hash((value >> 64) as u64);
    }
}

pub type FxBuildHasher = BuildHasherDefault<FxHasher>;

/// Drop-in `HashMap` with the fast hasher. Keys must not be
/// attacker-controlled (fine for guest pointers/SEL handles/class names).
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;

/// Drop-in `HashSet` with the fast hasher.
pub type FxHashSet<K> = std::collections::HashSet<K, FxBuildHasher>;
