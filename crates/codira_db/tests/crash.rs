//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The crash-injection harness. A deterministic workload (seeded) of
//!   transactions, explicit checkpoints and automatic checkpoints runs on a
//!   `FaultStorage` that crashes during its N-th mutating operation, for
//!   every N the workload performs. The crash image (unsynced bytes kept as
//!   a random prefix, unsynced directory operations kept as a random
//!   prefix) is then recovered, and the recovered state must be exactly the
//!   state after a prefix of the committed transactions:
//!   - with `Always`/`OnCommit`: every acknowledged commit, plus at most the
//!     one whose commit was in progress;
//!   - with `Never`: at least everything covered by the last successful
//!     explicit checkpoint, and nothing beyond the in-progress commit.
//! - Recovery must never report corruption for a crash image.
//! - A second crash *during recovery* must recover to the same state.
//! - After recovery, the database must accept new commits that survive a
//!   clean reopen (the torn tail was really cut off).

use std::{collections::BTreeMap, sync::Arc};

use codira_db::{fault::Rng, Db, Error, FaultStorage, FsyncPolicy, Options};

type State = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Clone, Debug)]
struct WTxn {
    ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    abort: bool,
    checkpoint_after: bool,
}

fn workload(seed: u64, n: usize) -> Vec<WTxn> {
    let mut rng = Rng::new(seed);
    (0..n)
        .map(|i| {
            let nops = 1 + rng.below(4) as usize;
            let ops = (0..nops)
                .map(|_| {
                    let key = format!("k{:02}", rng.below(12)).into_bytes();
                    if rng.below(5) == 0 {
                        (key, None)
                    } else {
                        let len = rng.below(40) as usize;
                        let fill = b'a' + rng.below(26) as u8;
                        let mut v = format!("t{i}:").into_bytes();
                        v.resize(v.len() + len, fill);
                        (key, Some(v))
                    }
                })
                .collect();
            WTxn {
                ops,
                abort: rng.below(10) == 0,
                checkpoint_after: rng.below(9) == 0,
            }
        })
        .collect()
}

/// States after 0, 1, 2, ... committed (non-aborted) transactions.
fn prefix_states(w: &[WTxn]) -> Vec<State> {
    let mut states = vec![State::new()];
    for t in w.iter().filter(|t| !t.abort) {
        let mut s = states.last().unwrap().clone();
        for (k, v) in &t.ops {
            match v {
                Some(v) => {
                    s.insert(k.clone(), v.clone());
                }
                None => {
                    s.remove(k);
                }
            }
        }
        states.push(s);
    }
    states
}

#[derive(Debug, Default)]
struct Outcome {
    /// Commits that returned `Ok`.
    acked: usize,
    /// A commit returned an I/O error: its outcome is unknown.
    in_flight: bool,
    /// Commits covered by the last explicit checkpoint that returned `Ok`.
    checkpointed: usize,
}

/// Counts of the cases the crash images hit, to check the harness covers
/// them.
#[derive(Debug, Default)]
struct Coverage {
    in_flight_recovered: usize,
    in_flight_lost: usize,
    acked_lost: usize,
    double_crashes: usize,
}

fn run(storage: &Arc<FaultStorage>, opts: &Options, w: &[WTxn]) -> Outcome {
    let mut out = Outcome::default();
    let Ok(db) = Db::open(storage.clone(), opts.clone()) else {
        return out;
    };
    for t in w {
        let mut txn = db.begin();
        for (k, v) in &t.ops {
            match v {
                Some(v) => txn.put(k, v).unwrap(),
                None => txn.delete(k).unwrap(),
            }
        }
        if t.abort {
            txn.abort();
            continue;
        }
        match txn.commit() {
            Ok(ts) => {
                out.acked += 1;
                assert_eq!(ts, out.acked as u64, "timestamps are dense");
            }
            Err(Error::Io(_)) => {
                out.in_flight = true;
                return out;
            }
            Err(Error::Poisoned) => return out,
            Err(e) => panic!("unexpected commit error: {e}"),
        }
        if t.checkpoint_after {
            match db.checkpoint() {
                Ok(()) => out.checkpointed = out.acked,
                Err(_) => return out,
            }
        }
    }
    let _ = db.close();
    out
}

fn contents(db: &Db) -> State {
    db.begin()
        .scan::<std::ops::RangeFull>(..)
        .into_iter()
        .collect()
}

fn options(policy: FsyncPolicy) -> Options {
    Options {
        fsync: policy,
        // Small enough that automatic checkpoints happen inside the
        // workload, so crashes land inside them too.
        checkpoint_wal_bytes: Some(700),
    }
}

/// Recovers `image` and returns the number of commits the recovered state
/// corresponds to, checking it against `outcome`.
fn recover_and_check(
    image: FaultStorage,
    policy: FsyncPolicy,
    states: &[State],
    outcome: &Outcome,
    what: &str,
    cov: &mut Coverage,
) -> (usize, Arc<FaultStorage>) {
    let image = Arc::new(image);
    let db = Db::open(image.clone(), options(policy))
        .unwrap_or_else(|e| panic!("{what}: recovery failed: {e}"));
    let got = contents(&db);
    // Timestamps are dense, so the recovered timestamp is the number of
    // commits recovered, and the contents must be exactly that prefix.
    let ts = db.recovery_report().last_commit_ts as usize;
    assert_eq!(
        states.get(ts),
        Some(&got),
        "{what}: recovered state is not the prefix of {ts} commits"
    );
    if outcome.in_flight {
        if ts > outcome.acked {
            cov.in_flight_recovered += 1;
        } else {
            cov.in_flight_lost += 1;
        }
    }
    if ts < outcome.acked {
        cov.acked_lost += 1;
    }
    let max = outcome.acked + usize::from(outcome.in_flight);
    let min = match policy {
        FsyncPolicy::Never => outcome.checkpointed,
        _ => outcome.acked,
    };
    assert!(
        (min..=max).contains(&ts),
        "{what}: recovered {ts} commits, expected {min}..={max} ({outcome:?})"
    );
    drop(db);
    (ts, image)
}

fn crash_everywhere(policy: FsyncPolicy, workload_seed: u64, seeds_per_point: u64) {
    let w = workload(workload_seed, 45);
    let states = prefix_states(&w);
    let opts = options(policy);

    let clean = Arc::new(FaultStorage::new());
    let full = run(&clean, &opts, &w);
    assert_eq!(full.acked, states.len() - 1);
    let total_ops = clean.ops();
    assert!(total_ops > 50, "workload too small: {total_ops} operations");

    let mut crash_points = 0;
    let mut cov = Coverage::default();
    for n in 0..=total_ops {
        for seed in 0..seeds_per_point {
            let storage = Arc::new(FaultStorage::new());
            storage.set_crash_at(Some(n), seed);
            let outcome = run(&storage, &opts, &w);
            let image_seed = n * 1000 + seed;
            let what = format!("{policy:?} crash at op {n}, seed {seed}");
            let (k, recovered) = recover_and_check(
                storage.crash_image(image_seed),
                policy,
                &states,
                &outcome,
                &what,
                &mut cov,
            );
            crash_points += 1;

            // A second crash during recovery of the same image recovers to
            // the same state.
            if seed == 0 {
                let probe = storage.crash_image(image_seed);
                let base = Arc::new(probe);
                let ops_before = base.ops();
                drop(Db::open(base.clone(), opts.clone()).unwrap());
                let recovery_ops = base.ops() - ops_before;
                for m in 0..recovery_ops {
                    let again = Arc::new(storage.crash_image(image_seed));
                    again.set_crash_at(Some(m), m);
                    assert!(Db::open(again.clone(), opts.clone()).is_err());
                    let (k2, _) = recover_and_check(
                        again.crash_image(m + 17),
                        policy,
                        &states,
                        &outcome,
                        &format!("{what}, then crash at recovery op {m}"),
                        &mut cov,
                    );
                    assert_eq!(k, k2, "{what}: double crash changed the outcome");
                    cov.double_crashes += 1;
                }
            }

            // The recovered database accepts new commits durably.
            if seed == 0 {
                let db = Db::open(recovered.clone(), opts.clone()).unwrap();
                let mut t = db.begin();
                t.put(b"after", format!("{n}").as_bytes()).unwrap();
                assert_eq!(t.commit().unwrap(), k as u64 + 1);
                db.close().unwrap();
                let db = Db::open(recovered, opts.clone()).unwrap();
                let mut expect = states[k].clone();
                expect.insert(b"after".to_vec(), format!("{n}").into_bytes());
                assert_eq!(contents(&db), expect, "{what}: commit after recovery lost");
            }
        }
    }
    eprintln!("{policy:?} workload {workload_seed}: {total_ops} operations, {crash_points} crash images, {cov:?}");
    // The harness must actually exercise the interesting cases.
    assert!(cov.in_flight_lost > 0 && cov.double_crashes > 0, "{cov:?}");
    if policy == FsyncPolicy::Never {
        // Under `Never` a commit's record is unsynced when its commit fails,
        // so recovering it needs the whole unsynced tail to survive: rare.
        assert!(cov.acked_lost > 0, "Never never lost a commit: {cov:?}");
    } else {
        // A crash in the fsync after a complete append can keep the record.
        assert!(cov.in_flight_recovered > 0, "{cov:?}");
        assert_eq!(cov.acked_lost, 0);
    }
    let files = codira_db::Storage::list(&*clean).unwrap();
    assert!(
        !files.contains(&"wal-00000000000000000001.log".to_string()),
        "no checkpoint happened: {files:?}"
    );
    assert!(crash_points as u64 > total_ops);
}

#[test]
fn crash_at_every_operation_always() {
    crash_everywhere(FsyncPolicy::Always, 1, 3);
}

#[test]
fn crash_at_every_operation_on_commit() {
    crash_everywhere(FsyncPolicy::OnCommit, 2, 3);
}

#[test]
fn crash_at_every_operation_never() {
    crash_everywhere(FsyncPolicy::Never, 3, 3);
}

#[test]
fn crash_at_every_operation_more_workloads() {
    for seed in 10..14 {
        crash_everywhere(FsyncPolicy::OnCommit, seed, 1);
    }
}

/// Under `Never`, a commit is not durable until a flush; with `OnCommit`
/// it is durable when `commit` returns, for every crash image.
#[test]
fn fsync_policy_decides_durability_at_commit() {
    for policy in [
        FsyncPolicy::Always,
        FsyncPolicy::OnCommit,
        FsyncPolicy::Never,
    ] {
        let storage = Arc::new(FaultStorage::new());
        let db = Db::open(storage.clone(), options(policy)).unwrap();
        let mut t = db.begin();
        t.put(b"k", b"v").unwrap();
        t.commit().unwrap();
        let mut lost = 0;
        for seed in 0..64 {
            let img = Arc::new(storage.crash_image(seed));
            let db2 = Db::open(img, options(policy)).unwrap();
            if db2.begin().get(b"k").is_none() {
                lost += 1;
            }
        }
        match policy {
            FsyncPolicy::Never => assert!(lost > 0, "Never must be able to lose a commit"),
            _ => assert_eq!(lost, 0, "{policy:?} lost an acknowledged commit"),
        }
        db.close().unwrap();
        for seed in 0..64 {
            let img = Arc::new(storage.crash_image(seed));
            let db2 = Db::open(img, options(policy)).unwrap();
            assert_eq!(db2.begin().get(b"k").as_deref(), Some(&b"v"[..]));
        }
    }
}
