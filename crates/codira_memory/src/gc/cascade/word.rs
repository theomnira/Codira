//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The 32-bit Cascade ownership word stored in every object header: the
//!   ownership state (unique, borrowed, shared, retiring, reclaimed), the
//!   publication flag, queue flags, the borrow count and the reference count
//!   of owning heap references, with saturating (sticky) arithmetic.
//!
//! The word is a plain value type here; who may load and store it is decided
//! by publication (see `CASCADE.md`, section 2): the owner thread while the
//! object is unpublished, the collector afterwards. Neither ever needs a
//! read-modify-write operation.

/// The ownership state of an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum OwnershipState {
    /// Not managed by Cascade: only the tracing collector reclaims it.
    Untracked = 0,
    /// Exactly one owning reference, held by its allocating thread.
    Unique = 1,
    /// Unique, with at least one active scoped borrow.
    Borrowed = 2,
    /// Counted: possibly several owning references.
    Shared = 3,
    /// The count reached zero; queued for validation and reclamation.
    Retiring = 4,
    /// The cell was freed (only observable on a freed small cell until it is
    /// reused).
    Reclaimed = 5,
}

const STATE_MASK: u32 = 0b111;
/// Reachable by more than one thread. Never cleared while the object lives.
pub(crate) const PUBLISHED: u32 = 1 << 3;
/// Queued in a zero-count table or the retire list.
pub(crate) const ZCT: u32 = 1 << 4;
/// Queued as a cycle candidate.
pub(crate) const BUFFERED: u32 = 1 << 5;
/// Member of a cycle set awaiting validation.
pub(crate) const CYCLE: u32 = 1 << 6;
/// Incremented since its cycle set was computed (the Δ-test).
pub(crate) const TOUCHED: u32 = 1 << 7;

const BORROW_SHIFT: u32 = 8;
const BORROW_MASK: u32 = 0b111 << BORROW_SHIFT;
/// The largest number of simultaneous borrows.
pub const MAX_BORROWS: u32 = 7;

const COUNT_SHIFT: u32 = 11;
/// The largest representable count. A count that reaches it is sticky.
pub const MAX_COUNT: u32 = u32::MAX >> COUNT_SHIFT;

/// A decoded ownership word.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Word(pub(crate) u32);

impl std::fmt::Debug for Word {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Word")
            .field("state", &self.state())
            .field("count", &self.count())
            .field("borrows", &self.borrows())
            .field("published", &self.is(PUBLISHED))
            .field("zct", &self.is(ZCT))
            .field("buffered", &self.is(BUFFERED))
            .field("cycle", &self.is(CYCLE))
            .field("touched", &self.is(TOUCHED))
            .finish()
    }
}

impl Word {
    /// The word of an object Cascade does not manage.
    pub const UNTRACKED: Word = Word(0);

    /// The word of a freshly allocated, unpublished, unique object.
    pub const fn new_unique() -> Word {
        Word(OwnershipState::Unique as u32)
    }

    /// Returns the raw bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns the ownership state.
    pub fn state(self) -> OwnershipState {
        match self.0 & STATE_MASK {
            0 => OwnershipState::Untracked,
            1 => OwnershipState::Unique,
            2 => OwnershipState::Borrowed,
            3 => OwnershipState::Shared,
            4 => OwnershipState::Retiring,
            _ => OwnershipState::Reclaimed,
        }
    }

    /// Returns the word with `state` instead of the current state.
    #[must_use]
    pub fn with_state(self, state: OwnershipState) -> Word {
        Word((self.0 & !STATE_MASK) | state as u32)
    }

    /// Whether Cascade manages the object.
    pub fn is_tracked(self) -> bool {
        !matches!(
            self.state(),
            OwnershipState::Untracked | OwnershipState::Reclaimed
        )
    }

    /// Whether all bits of `flag` are set.
    pub fn is(self, flag: u32) -> bool {
        self.0 & flag == flag
    }

    /// Returns the word with `flag` set.
    #[must_use]
    pub fn set(self, flag: u32) -> Word {
        Word(self.0 | flag)
    }

    /// Returns the word with `flag` cleared.
    #[must_use]
    pub fn clear(self, flag: u32) -> Word {
        Word(self.0 & !flag)
    }

    /// Whether the object is reachable by more than one thread.
    pub fn is_published(self) -> bool {
        self.is(PUBLISHED)
    }

    /// The count of owning heap references.
    pub fn count(self) -> u32 {
        self.0 >> COUNT_SHIFT
    }

    /// Whether the count saturated: it no longer changes, and only the
    /// tracing collector can reclaim the object.
    pub fn is_sticky(self) -> bool {
        self.count() == MAX_COUNT
    }

    /// Returns the word with the count set to `count` (saturated).
    #[must_use]
    pub fn with_count(self, count: u32) -> Word {
        Word((self.0 & ((1 << COUNT_SHIFT) - 1)) | (count.min(MAX_COUNT) << COUNT_SHIFT))
    }

    /// Returns the word with the count incremented; a sticky count stays.
    #[must_use]
    pub fn inc(self) -> Word {
        if self.is_sticky() {
            self
        } else {
            Word(self.0 + (1 << COUNT_SHIFT))
        }
    }

    /// Returns the word with the count decremented; a sticky count stays.
    /// Decrementing a zero count is a protocol error: it would mean a
    /// reference was released twice. Debug builds panic; release builds keep
    /// the count at zero.
    #[must_use]
    pub fn dec(self) -> Word {
        if self.is_sticky() {
            return self;
        }
        debug_assert!(self.count() > 0, "Cascade count underflow: {self:?}");
        if self.count() == 0 {
            self
        } else {
            Word(self.0 - (1 << COUNT_SHIFT))
        }
    }

    /// The number of active borrows.
    pub fn borrows(self) -> u32 {
        (self.0 & BORROW_MASK) >> BORROW_SHIFT
    }

    /// Returns the word with the borrow count set to `borrows`.
    #[must_use]
    pub fn with_borrows(self, borrows: u32) -> Word {
        debug_assert!(borrows <= MAX_BORROWS);
        Word((self.0 & !BORROW_MASK) | (borrows.min(MAX_BORROWS) << BORROW_SHIFT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_independent() {
        let states = [
            OwnershipState::Untracked,
            OwnershipState::Unique,
            OwnershipState::Borrowed,
            OwnershipState::Shared,
            OwnershipState::Retiring,
            OwnershipState::Reclaimed,
        ];
        let flags = [PUBLISHED, ZCT, BUFFERED, CYCLE, TOUCHED];
        for &state in &states {
            for mask in 0..(1u32 << flags.len()) {
                for borrows in 0..=MAX_BORROWS {
                    for count in [0, 1, 2, 1000, MAX_COUNT - 1, MAX_COUNT] {
                        let mut word = Word::UNTRACKED
                            .with_state(state)
                            .with_borrows(borrows)
                            .with_count(count);
                        for (i, &flag) in flags.iter().enumerate() {
                            if mask & (1 << i) != 0 {
                                word = word.set(flag);
                            }
                        }
                        assert_eq!(word.state(), state);
                        assert_eq!(word.borrows(), borrows);
                        assert_eq!(word.count(), count);
                        for (i, &flag) in flags.iter().enumerate() {
                            assert_eq!(word.is(flag), mask & (1 << i) != 0);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn counts_saturate_and_stick() {
        let word = Word::new_unique().with_count(MAX_COUNT - 1);
        let word = word.inc();
        assert!(word.is_sticky());
        assert_eq!(word.inc(), word);
        assert_eq!(word.dec(), word);
        assert_eq!(Word::new_unique().with_count(u32::MAX).count(), MAX_COUNT);
    }

    #[test]
    fn inc_dec_round_trip_keeps_flags() {
        let word = Word::new_unique()
            .with_state(OwnershipState::Shared)
            .set(PUBLISHED | BUFFERED)
            .with_borrows(3);
        assert_eq!(word.inc().dec(), word);
        assert_eq!(word.inc().count(), 1);
    }
}
