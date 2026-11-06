//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - `loom` models of the Cascade epoch protocol (handshakes, boundaries,
//!   quiescence) and of the publication hand-off. Run with
//!   `RUSTFLAGS="--cfg loom" cargo test -p codira_memory --lib --release
//!   cascade::loom_tests`.

use loom::{
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Arc,
    },
    thread,
};

use super::epoch::EpochCore;

fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(f);
}

/// A boundary that begins after both mutators handshook into it sees both
/// packets (a push happens-before its announcement), and a packet's segment
/// never exceeds the boundary that drains it.
#[test]
fn boundary_sees_every_handshake_packet() {
    model(|| {
        let core = Arc::new(EpochCore::<(usize, u64, u64, bool)>::new());
        let slots = [core.register(), core.register()];
        // Boundary 1 needs nobody: both registered at epoch 1.
        let (boundary, _) = core.try_begin_boundary().expect("boundary 1");
        assert_eq!(boundary, 1);
        core.finish_boundary(1);

        let handles: Vec<_> = slots
            .into_iter()
            .enumerate()
            .map(|(i, slot)| {
                let core = core.clone();
                thread::spawn(move || {
                    let h = core.handshake(&slot, false, |h| {
                        Some((i, h.segment, h.epoch, h.new_epoch))
                    });
                    assert_eq!(h.epoch, 2);
                    assert!(h.new_epoch);
                })
            })
            .collect();

        let mut seen = Vec::new();
        loop {
            if let Some((boundary, packets)) = core.try_begin_boundary() {
                assert_eq!(boundary, 2);
                seen.extend(packets);
                break;
            }
            thread::yield_now();
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let mut threads: Vec<usize> = seen.iter().map(|p| p.0).collect();
        threads.sort_unstable();
        assert_eq!(threads, vec![0, 1], "a handshake's packet was missing");
        for packet in &seen {
            assert!(packet.1 <= 2 && packet.2 == 2 && packet.3);
        }
    });
}

/// A quiescent mutator does not hold up the epoch, and the packet it handed
/// over when it became quiescent is drained no later than the first
/// boundary that relied on its quiescence.
#[test]
fn quiescent_snapshot_precedes_the_boundary_relying_on_it() {
    model(|| {
        let core = Arc::new(EpochCore::<&'static str>::new());
        let slot = core.register();
        let (b, _) = core.try_begin_boundary().unwrap();
        core.finish_boundary(b);

        let mutator = {
            let core = core.clone();
            thread::spawn(move || {
                core.handshake(&slot, true, |_| Some("quiescent"));
                core.handshake(&slot, false, |_| Some("resumed"));
            })
        };

        let mut drained = Vec::new();
        let mut began = None;
        for _ in 0..3 {
            if let Some((boundary, packets)) = core.try_begin_boundary() {
                drained.extend(packets);
                began = Some(boundary);
                core.finish_boundary(boundary);
                break;
            }
            thread::yield_now();
        }
        if began.is_some() {
            // Boundary 2 began: the mutator announced 2 (either handshake)
            // or was quiescent, and in both cases had pushed "quiescent".
            assert!(drained.contains(&"quiescent"));
        }
        mutator.join().unwrap();
    });
}

/// Publication: the owner's last plain store of the ownership word, then
/// the release store setting `PUBLISHED`, then the release of the object
/// into a shared field. A thread that loads it logs an increment and hands
/// it to the collector through the inbox; the collector, the word's next
/// writer, must see the owner's final word.
#[test]
fn publication_hands_the_word_to_the_collector() {
    const PUBLISHED: u32 = 1 << 3;
    model(|| {
        let core = Arc::new(EpochCore::<usize>::new());
        let word = Arc::new(AtomicU32::new(0));
        let field = Arc::new(AtomicUsize::new(0));
        let reader_slot = core.register();

        let owner = {
            let (word, field) = (word.clone(), field.clone());
            thread::spawn(move || {
                // Unpublished: plain (relaxed) bookkeeping by the owner.
                word.store(5 << 11, Ordering::Relaxed);
                word.store(word.load(Ordering::Relaxed) | PUBLISHED, Ordering::Release);
                field.swap(1, Ordering::AcqRel);
            })
        };
        let reader = {
            let (core, field) = (core.clone(), field.clone());
            thread::spawn(move || {
                let value = field.load(Ordering::Acquire);
                core.handshake(&reader_slot, false, |_| (value != 0).then_some(value));
            })
        };
        // The collector runs concurrently: it drains the inbox and then
        // writes the word (the increment), as a boundary would.
        let collector = {
            let (core, word) = (core.clone(), word.clone());
            thread::spawn(move || {
                for _ in 0..2 {
                    for packet in core.drain() {
                        assert_eq!(packet, 1);
                        let seen = word.load(Ordering::Relaxed);
                        assert_eq!(seen, (5 << 11) | PUBLISHED, "collector saw a stale word");
                        word.store(seen + (1 << 11), Ordering::Relaxed);
                    }
                    thread::yield_now();
                }
            })
        };
        owner.join().unwrap();
        reader.join().unwrap();
        collector.join().unwrap();
    });
}

/// The collector reads announcements and drains concurrently with a mutator
/// that handshakes twice: every drained packet's segment is at most the
/// boundary draining it, and the epoch only moves forward.
#[test]
fn segments_never_exceed_their_boundary() {
    model(|| {
        let core = Arc::new(EpochCore::<u64>::new());
        let slot = core.register();
        let mutator = {
            let core = core.clone();
            thread::spawn(move || {
                for _ in 0..2 {
                    core.handshake(&slot, false, |h| Some(h.segment));
                }
            })
        };
        let mut last = 0;
        for _ in 0..2 {
            if let Some((boundary, packets)) = core.try_begin_boundary() {
                assert!(boundary > last);
                last = boundary;
                for segment in packets {
                    assert!(segment <= boundary);
                }
                core.finish_boundary(boundary);
            }
        }
        mutator.join().unwrap();
    });
}
