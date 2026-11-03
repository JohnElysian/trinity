//! Trinity 45017's temporary collision-candidate container, not a global ID sort.
//!
//! Native constructor 0x08b50 starts one active bucket and mask 1. Insertion
//! 0x06ec0 splits one bucket before lookup whenever active_buckets <= size/4;
//! even a duplicate insertion can cause that split. Iteration visits buckets
//! in order, with signed 32-bit ball IDs ascending inside each bucket.
use std::collections::BTreeMap;

pub(super) fn order(ids: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let mut unique = BTreeMap::new();
    let (mut mask, mut buckets) = (1u32, 1usize);
    for id in ids {
        if buckets <= unique.len() / 4 {
            if (mask as usize) < buckets {
                mask = mask * 2 + 1;
            }
            buckets += 1;
        }
        // Native ball IDs and candidate keys are signed 32-bit values. Keep
        // the first payload for a repeated key, as native insertion does.
        unique.entry(id as i32).or_insert(id);
    }
    let mut ordered: Vec<_> = unique.into_iter().collect();
    ordered.sort_unstable_by_key(|(key, _)| {
        let mut bucket = hash(*key) & mask;
        if bucket as usize >= buckets {
            bucket -= (mask >> 1) + 1;
        }
        (bucket, *key)
    });
    ordered.into_iter().map(|(_, id)| id).collect()
}

/// RVA 0x071d9: MSVC ldiv truncates toward zero, including negative seeds.
/// The Park–Miller step fits in a signed i32; i64 avoids incidental overflow.
pub(super) fn hash(id: i32) -> u32 {
    let seed = i64::from((id as u32 ^ 0xdead_beef) as i32);
    let quotient = seed / 127_773;
    let remainder = seed % 127_773;
    let value = 16_807 * remainder - 2_836 * quotient;
    if value < 0 { (value + 0x7fff_ffff) as u32 } else { value as u32 }
}

/// The same linear hash table with unsigned pointer keys. Erasing an entry
/// does not shrink its buckets; native clear() resets them to one.
#[derive(Debug, Clone)]
pub(super) struct PointerSet<T> {
    entries: BTreeMap<u32, T>,
    mask: u32,
    buckets: usize,
}

impl<T> Default for PointerSet<T> {
    fn default() -> Self { Self { entries: BTreeMap::new(), mask: 1, buckets: 1 } }
}

impl<T: Copy> PointerSet<T> {
    pub fn insert(&mut self, token: u32, value: T) {
        if self.buckets <= self.entries.len() / 4 {
            if (self.mask as usize) < self.buckets { self.mask = self.mask * 2 + 1; }
            self.buckets += 1;
        }
        self.entries.entry(token).or_insert(value);
    }
    pub fn remove(&mut self, token: u32) { self.entries.remove(&token); }
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn ordered(&self) -> Vec<T> {
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_unstable_by_key(|(token, _)| {
            let mut bucket = hash(**token as i32) & self.mask;
            if bucket as usize >= self.buckets { bucket -= (self.mask >> 1) + 1; }
            (bucket, **token)
        });
        entries.into_iter().map(|(_, value)| *value).collect()
    }
}
