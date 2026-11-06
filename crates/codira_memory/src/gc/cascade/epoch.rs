//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The Cascade epoch protocol: mutator registration, handshakes that hand
//!   a packet to the collector and announce the epoch, quiescence, and the
//!   collector's check that every mutator announced the current epoch
//!   before a boundary is processed.
//!
//! The protocol is isolated from the heap so that `loom` can check it
//! (`loom_tests.rs`). Its guarantees, used by `CASCADE.md` section 5:
//!
//! 1. When [`EpochCore::try_begin_boundary`] returns boundary `b`, every
//!    mutator registered at that moment either announced `b` or is
//!    quiescent, and the packets it pushed before announcing are among the
//!    drained packets (a push happens-before its announcement, which the
//!    collector reads with acquire ordering before draining).
//! 2. The epoch only advances in [`EpochCore::finish_boundary`], so every
//!    announcement is `E` or `E - 1`, and a packet's segment is at most the
//!    boundary that drains it.
//! 3. A thread registered after the check of boundary `b` started cannot
//!    have announced anything the collector relied on: registration and the
//!    check both take the registry lock.

use super::sync::{lock, Arc, AtomicU64, Mutex, Ordering};

/// Announcement bit: the thread is quiescent.
const QUIESCENT: u64 = 1;

/// A mutator's announcement slot.
pub(crate) struct Slot {
    /// `epoch << 1 | quiescent`.
    announced: AtomicU64,
}

impl Slot {
    /// The epoch the thread last announced.
    pub(crate) fn epoch(&self) -> u64 {
        self.announced.load(Ordering::Relaxed) >> 1
    }
}

/// What a handshake knows when it builds its packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Handshake {
    /// The epoch the thread announced before this handshake: the segment of
    /// the operations it logged since.
    pub segment: u64,
    /// The epoch this handshake announces.
    pub epoch: u64,
    /// Whether this handshake enters a new epoch, so the packet must carry
    /// a root snapshot for `epoch`.
    pub new_epoch: bool,
}

/// The global epoch, the registry and the collector's inbox.
pub(crate) struct EpochCore<P> {
    epoch: AtomicU64,
    slots: Mutex<Vec<Arc<Slot>>>,
    inbox: Mutex<Vec<P>>,
}

impl<P> EpochCore<P> {
    pub(crate) fn new() -> Self {
        Self {
            epoch: AtomicU64::new(1),
            slots: Mutex::new(Vec::new()),
            inbox: Mutex::new(Vec::new()),
        }
    }

    /// The current global epoch.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Registers a mutator; it announces the current epoch.
    pub(crate) fn register(&self) -> Arc<Slot> {
        let mut slots = lock(&self.slots);
        let slot = Arc::new(Slot {
            announced: AtomicU64::new(self.epoch.load(Ordering::Acquire) << 1),
        });
        slots.push(slot.clone());
        slot
    }

    /// Unregisters a mutator. Its final packet must have been pushed.
    pub(crate) fn unregister(&self, slot: &Arc<Slot>) {
        lock(&self.slots).retain(|s| !Arc::ptr_eq(s, slot));
    }

    /// The number of registered mutators.
    pub(crate) fn registered(&self) -> usize {
        lock(&self.slots).len()
    }

    /// Performs a handshake: builds a packet with `make`, pushes it, then
    /// announces the epoch (quiescent if `quiescent`). Returns what `make`
    /// was given.
    pub(crate) fn handshake(
        &self,
        slot: &Slot,
        quiescent: bool,
        make: impl FnOnce(Handshake) -> Option<P>,
    ) -> Handshake {
        let previous = slot.announced.load(Ordering::Relaxed);
        let epoch = self.epoch.load(Ordering::Acquire);
        let handshake = Handshake {
            segment: previous >> 1,
            epoch,
            // Leaving quiescence also needs a fresh snapshot.
            new_epoch: epoch != previous >> 1 || previous & QUIESCENT != 0,
        };
        if let Some(packet) = make(handshake) {
            lock(&self.inbox).push(packet);
        }
        let flag = if quiescent { QUIESCENT } else { 0 };
        slot.announced.store(epoch << 1 | flag, Ordering::Release);
        handshake
    }

    /// Pushes a packet without announcing (a full buffer mid-segment).
    pub(crate) fn push(&self, packet: P) {
        lock(&self.inbox).push(packet);
    }

    /// If every registered mutator announced the current epoch `b` or is
    /// quiescent, drains the inbox and returns `b` and the packets.
    pub(crate) fn try_begin_boundary(&self) -> Option<(u64, Vec<P>)> {
        let epoch = self.epoch.load(Ordering::Acquire);
        {
            let slots = lock(&self.slots);
            for slot in slots.iter() {
                let announced = slot.announced.load(Ordering::Acquire);
                if announced & QUIESCENT == 0 && announced >> 1 != epoch {
                    return None;
                }
            }
        }
        let packets = std::mem::take(&mut *lock(&self.inbox));
        Some((epoch, packets))
    }

    /// Drains the inbox without checking announcements (used while the world
    /// is stopped, when every packet describes changes already in the heap).
    pub(crate) fn drain(&self) -> Vec<P> {
        std::mem::take(&mut *lock(&self.inbox))
    }

    /// Ends boundary `b`: the epoch becomes `b + 1`.
    pub(crate) fn finish_boundary(&self, boundary: u64) {
        debug_assert_eq!(self.epoch.load(Ordering::Relaxed), boundary);
        self.epoch.store(boundary + 1, Ordering::Release);
    }
}
