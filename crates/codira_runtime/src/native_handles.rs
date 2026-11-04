//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The generation-checked handle table behind the database
//!   (`native_db`) and tensor (`native_ai`) services: the same scheme as the
//!   file handles of `native_fs`, made generic and given a *kind* tag so that
//!   a handle of one kind (a transaction) can never be mistaken for a handle
//!   of another (a database) even though each kind has its own table.
//!
//! A handle is a positive `i64`:
//!
//! | bits   | contents                                         |
//! |--------|--------------------------------------------------|
//! | 0..20  | slot index plus one (so `0` is never a handle)   |
//! | 20..56 | generation of the slot, bumped at every removal  |
//! | 56..63 | kind tag (non-zero; one per table)               |
//! | 63     | zero (handles are positive)                      |
//!
//! A handle is accepted only if its tag is the table's, its slot exists, the
//! slot is occupied and the slot's generation matches. A removed handle, a
//! handle of another kind, a forged number and `0` or a negative number are
//! all refused. The file handles of `native_fs` have a zero tag field (their
//! generation would have to pass 2^36 reuses of one slot to reach it), so
//! they are refused here too.

/// Bits of a handle that hold the slot index plus one.
const INDEX_BITS: u32 = 20;
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;
/// Bits of a handle that hold the generation.
const GENERATION_BITS: u32 = 36;
const GENERATION_MASK: u64 = (1 << GENERATION_BITS) - 1;
/// Where the kind tag starts; seven bits, below the sign bit.
const TAG_SHIFT: u32 = INDEX_BITS + GENERATION_BITS;
const TAG_MASK: u64 = (1 << (63 - TAG_SHIFT)) - 1;

/// The most entries one table holds at once.
pub(crate) const MAX_ENTRIES: usize = INDEX_MASK as usize - 1;

struct Slot<T> {
    generation: u64,
    value: Option<T>,
}

/// A table of values addressed by generation-checked handles of one kind.
pub(crate) struct HandleTable<T> {
    tag: u64,
    slots: Vec<Slot<T>>,
    free: Vec<usize>,
}

impl<T> HandleTable<T> {
    /// An empty table whose handles carry `tag` (`1..=127`, distinct per
    /// table).
    pub(crate) const fn new(tag: u8) -> Self {
        assert!(tag != 0 && (tag as u64) <= TAG_MASK);
        HandleTable {
            tag: tag as u64,
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    fn handle(&self, index: usize, generation: u64) -> i64 {
        // `index < INDEX_MASK`, `generation <= GENERATION_MASK` and
        // `tag <= TAG_MASK`, so this is positive and fits.
        ((self.tag << TAG_SHIFT) | (generation << INDEX_BITS) | (index as u64 + 1)) as i64
    }

    /// The slot index of a live handle of this table.
    fn index(&self, handle: i64) -> Option<usize> {
        let handle = u64::try_from(handle).ok()?;
        if handle >> TAG_SHIFT != self.tag {
            return None;
        }
        let index = usize::try_from(handle & INDEX_MASK).ok()?.checked_sub(1)?;
        let slot = self.slots.get(index)?;
        let generation = (handle >> INDEX_BITS) & GENERATION_MASK;
        (slot.generation == generation && slot.value.is_some()).then_some(index)
    }

    /// The value of a live handle.
    pub(crate) fn get(&self, handle: i64) -> Option<&T> {
        let index = self.index(handle)?;
        self.slots[index].value.as_ref()
    }

    /// Stores `value`, returning its handle, or gives it back when the table
    /// is full.
    pub(crate) fn insert(&mut self, value: T) -> Result<i64, T> {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index];
            slot.value = Some(value);
            let generation = slot.generation;
            Ok(self.handle(index, generation))
        } else if self.slots.len() < MAX_ENTRIES {
            self.slots.push(Slot {
                generation: 0,
                value: Some(value),
            });
            Ok(self.handle(self.slots.len() - 1, 0))
        } else {
            Err(value)
        }
    }

    /// Removes a live handle's value; the handle is refused from then on.
    pub(crate) fn remove(&mut self, handle: i64) -> Option<T> {
        let index = self.index(handle)?;
        let slot = &mut self.slots[index];
        let value = slot.value.take();
        slot.generation = slot.generation.wrapping_add(1) & GENERATION_MASK;
        self.free.push(index);
        value
    }

    /// The number of live entries.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_positive_tagged_and_generation_checked() {
        let mut a = HandleTable::new(1);
        let mut b = HandleTable::new(2);
        let first = a.insert("first").unwrap();
        let other = b.insert("other").unwrap();
        assert!(first > 0 && other > 0);
        // The same slot in two tables gives two different handles, and
        // neither table accepts the other's.
        assert_ne!(first, other);
        assert!(a.get(other).is_none());
        assert!(b.get(first).is_none());
        assert_eq!(a.get(first), Some(&"first"));

        assert_eq!(a.remove(first), Some("first"));
        assert!(a.get(first).is_none());
        assert!(a.remove(first).is_none());
        // The slot is reused with a new generation; the stale handle stays
        // refused.
        let second = a.insert("second").unwrap();
        assert_ne!(first, second);
        assert!(a.get(first).is_none());
        assert_eq!(a.get(second), Some(&"second"));
        assert_eq!(a.len(), 1);

        for forged in [0, -1, i64::MIN, i64::MAX, second + 1, second ^ (1 << 30)] {
            assert!(a.get(forged).is_none(), "{forged}");
        }
        // A `native_fs`-style handle (tag field zero) is refused.
        assert!(a.get(1).is_none());
    }

    #[test]
    fn generations_wrap_without_leaving_the_mask() {
        let mut table = HandleTable::new(127);
        table.insert(0u8).unwrap();
        // Age the slot to its last generation and re-derive its handle.
        table.slots[0].generation = GENERATION_MASK;
        let handle = table.handle(0, GENERATION_MASK);
        assert_eq!(table.get(handle), Some(&0));
        assert_eq!(table.remove(handle), Some(0));
        let next = table.insert(1).unwrap();
        assert!(next > 0);
        assert_eq!(table.get(next), Some(&1));
        assert!(table.get(handle).is_none());
    }
}
