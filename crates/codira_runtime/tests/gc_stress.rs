//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The GC stress gate: seeded random object graphs built and mutated by
//!   Codira code and through the host API, with collections forced at
//!   random points -- from inside Codira code, from a host thread, and by a
//!   heap limit -- while a shadow model in Rust, run on the same seed, is
//!   the oracle.
//!
//! # The oracle
//!
//! A Codira program (`GRAPH`) keeps a table of slots holding nodes and
//! applies a stream of random operations to it: new nodes, edges (cycles
//! and self-loops included), edge removal, promoting a deep node to a slot,
//! bursts of garbage, and spot checks. Every node carries its id, a check
//! value and a label string derived from the id. The random stream comes
//! from a generator written in Codira, and [`Shadow`] replays it in Rust
//! over a model of the same graph. Periodically the program walks
//! everything reachable from the slots in a fixed order, checks every
//! node's contents and folds the ids into a digest; the run passes only if
//! the program saw no bad node and its digest equals the shadow's.
//!
//! A collection that frees a reachable object shows up as a bad node (its
//! cell reused by a garbage node with other contents), as a different
//! digest (a reused cell with another id, or lost edges), or as a crash. A
//! final collection with nothing rooted must free every byte: anything
//! left is a leak.
//!
//! # Run length
//!
//! Each test runs seeds until `CODIRA_STRESS_SECS` seconds (default 3) have
//! passed, and reports the seeds and collections it ran.
//! `CODIRA_STRESS_SEED` sets the first seed (default 1; every failure names
//! its seed, so it can be replayed). The gate runner
//! (`scripts/aurora_gates.py`) runs it for minutes; run it for hours with
//! `CODIRA_STRESS_SECS=7200`.
//!
//! # What it cannot catch
//!
//! Memory: with a heap limit the limit enforces the bound; with pacing
//! alone `pacing_alone_keeps_codira_graphs_bounded` checks the peak against
//! a fixed bound; every test checks for leaks after the run.
//! The conservative stack scan may keep garbage alive, which no oracle here
//! can tell from a leak while the program runs. Bugs that need a particular
//! interleaving the scheduler never produces in the time given are missed.
use std::{
    sync::{
        atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use codira_memory::gc::{Event, GcPtr, GcRuntime, HasIndirectionPtr, MarkSweep, NoopObserver};
use codira_runtime::{RootedStruct, StructRef};
use codira_test::CompileAndRunTestDriver;

const GRAPH: &str = r#"
    extern func stress_point(op: i64) -> i64;

    public class GNode { id: i64, check: i64, mark: i64, kids: [GNode], label: String }
    public class Rng { state: i64 }
    public class Acc { digest: i64, bad: i64 }

    func check_of(id: i64) -> i64 { (id * 2654435761) % 1000000007 }
    func label_of(id: i64) -> String { "n" + id.to_string() }

    func next(r: Rng) -> i64 {
        r.state = (r.state * 1103515245 + 12345) % 2147483648
        r.state / 65536
    }

    func make_node(id: i64) -> GNode {
        GNode { id: id, check: check_of(id), mark: 0, kids: [], label: label_of(id) }
    }

    func bad_node(n: GNode) -> i64 {
        if n.check != check_of(n.id) || n.label != label_of(n.id) || n.kids.len() > 4 { 1 } else { 0 }
    }

    // Visits everything reachable from `slots` depth first, in a fixed
    // order, checking each node and folding its id into the digest.
    func walk(slots: [GNode], epoch: i64, acc: Acc) {
        let stack: [GNode] = []
        for s in slots {
            stack.push(s)
            while stack.len() > 0 {
                let n = stack.pop()!
                if n.mark != epoch {
                    n.mark = epoch
                    acc.bad += bad_node(n)
                    acc.digest = (acc.digest * 131 + n.id + 1) % 1000000007
                    for kid in n.kids { stack.push(kid) }
                }
            }
        }
    }

    public func stress(seed: i64, slots_n: i64, ops: i64) -> i64 {
        let rng = Rng { state: seed }
        let acc = Acc { digest: 0, bad: 0 }
        let slots: [GNode] = []
        var next_id = 0
        while next_id < slots_n {
            slots.push(make_node(next_id))
            next_id += 1
        }
        var epoch = 0
        var op = 0
        while op < ops {
            let r = next(rng) % 100
            let a = (next(rng) % slots_n) as usize
            if r < 30 {
                slots[a] = make_node(next_id)
                next_id += 1
            } else if r < 55 {
                let b = (next(rng) % slots_n) as usize
                let x = slots[a]
                if x.kids.len() < 4 {
                    x.kids.push(slots[b])
                } else {
                    x.kids[(next(rng) % 4) as usize] = slots[b]
                }
            } else if r < 62 {
                let x = slots[a]
                if x.kids.len() < 4 { x.kids.push(x) } else { x.kids[0] = x }
            } else if r < 70 {
                let gone = slots[a].kids.pop()
                if let g = gone { acc.bad += bad_node(g) }
            } else if r < 85 {
                let k = next(rng) % 16 + 1
                let chain: [GNode] = []
                var j = 0
                while j < k {
                    let g = make_node(0 - j - 1)
                    if j > 0 { g.kids.push(chain[(j - 1) as usize]) }
                    chain.push(g)
                    j += 1
                }
                for g in chain { acc.bad += bad_node(g) }
            } else if r < 95 {
                let x = slots[a]
                acc.bad += bad_node(x)
                for kid in x.kids { acc.bad += bad_node(kid) }
            } else {
                let b = (next(rng) % slots_n) as usize
                let y = slots[b]
                if y.kids.len() > 0 { slots[a] = y.kids[0] }
            }
            stress_point(op)
            if op % 512 == 511 || op == ops - 1 {
                epoch += 1
                walk(slots, epoch, acc)
            }
            op += 1
        }
        if acc.bad > 0 { 0 - acc.bad } else { acc.digest }
    }

    func worker(results: Channel[i64], seed: i64, slots_n: i64, ops: i64) {
        results <- stress(seed, slots_n, ops)
    }

    // `tasks` graphs on worker threads and one on this thread; the sum of
    // their digests, or the most negative result if any saw a bad node.
    public func stress_tasks(seed: i64, slots_n: i64, ops: i64, tasks: i64) -> i64 {
        scope {
            let results = channel(16)
            var t = 0
            while t < tasks {
                spawn worker(results, seed + t + 1, slots_n, ops)
                t += 1
            }
            var worst = stress(seed, slots_n, ops)
            var sum = if worst > 0 { worst } else { 0 }
            t = 0
            while t < tasks {
                let d = <-results
                if d < worst { worst = d }
                if d > 0 { sum = (sum + d) % 1000000007 }
                t += 1
            }
            if worst < 0 { worst } else { sum }
        }
    }

    // Helpers for the host-API stress.
    public func new_node(id: i64) -> GNode { make_node(id) }
    public func link(x: GNode, y: GNode, at: i64) {
        if x.kids.len() < 4 { x.kids.push(y) } else { x.kids[at as usize] = y }
    }
    public func unlink(x: GNode) -> i64 {
        let gone = x.kids.pop()
        if let g = gone { bad_node(g) } else { 0 }
    }
    public func kid_count(x: GNode) -> i64 { x.kids.len() as i64 }
    public func first_kid(x: GNode) -> GNode { x.kids[0] }
    public func node_bad(x: GNode) -> i64 { bad_node(x) }
    public func walk_from(x: GNode, epoch: i64) -> i64 {
        let acc = Acc { digest: 0, bad: 0 }
        let one: [GNode] = []
        one.push(x)
        walk(one, epoch, acc)
        if acc.bad > 0 { 0 - acc.bad } else { acc.digest }
    }
"#;

/// The runtime's collector type (`Runtime::gc`).
type GarbageCollector = MarkSweep<NoopObserver<Event>>;

const MODULUS: i64 = 1_000_000_007;

fn check_of(id: i64) -> i64 {
    id.wrapping_mul(2_654_435_761) % MODULUS
}

/// The generator of `GRAPH`, replayed.
struct Rng(i64);

impl Rng {
    fn next(&mut self) -> i64 {
        self.0 = (self.0 * 1_103_515_245 + 12_345) % 2_147_483_648;
        self.0 / 65_536
    }
}

/// The shadow model: `GRAPH`'s graph without the GC. Nodes are never freed.
struct Shadow {
    ids: Vec<i64>,
    kids: Vec<Vec<usize>>,
    slots: Vec<usize>,
}

impl Shadow {
    fn new() -> Self {
        Self {
            ids: Vec::new(),
            kids: Vec::new(),
            slots: Vec::new(),
        }
    }

    fn node(&mut self, id: i64) -> usize {
        self.ids.push(id);
        self.kids.push(Vec::new());
        self.ids.len() - 1
    }

    /// The digest of everything reachable from `roots`, in `walk`'s order.
    fn walk(&self, roots: &[usize], digest: &mut i64) {
        let mut seen = vec![false; self.ids.len()];
        let mut stack = Vec::new();
        for &root in roots {
            stack.push(root);
            while let Some(n) = stack.pop() {
                if !seen[n] {
                    seen[n] = true;
                    *digest = (*digest * 131 + self.ids[n] + 1) % MODULUS;
                    stack.extend(self.kids[n].iter().copied());
                }
            }
        }
    }

    /// What `stress(seed, slots_n, ops)` must return.
    fn stress(seed: i64, slots_n: i64, ops: i64) -> i64 {
        let mut rng = Rng(seed);
        let mut m = Shadow::new();
        let mut next_id = 0;
        while next_id < slots_n {
            let n = m.node(next_id);
            m.slots.push(n);
            next_id += 1;
        }
        let mut digest = 0;
        for op in 0..ops {
            let r = rng.next() % 100;
            let a = (rng.next() % slots_n) as usize;
            if r < 30 {
                m.slots[a] = m.node(next_id);
                next_id += 1;
            } else if r < 55 {
                let b = (rng.next() % slots_n) as usize;
                let (x, y) = (m.slots[a], m.slots[b]);
                if m.kids[x].len() < 4 {
                    m.kids[x].push(y);
                } else {
                    let at = (rng.next() % 4) as usize;
                    m.kids[x][at] = y;
                }
            } else if r < 62 {
                let x = m.slots[a];
                if m.kids[x].len() < 4 {
                    m.kids[x].push(x);
                } else {
                    m.kids[x][0] = x;
                }
            } else if r < 70 {
                let x = m.slots[a];
                m.kids[x].pop();
            } else if r < 85 {
                let _k = rng.next() % 16 + 1;
            } else if r < 95 {
                // A spot check: nothing to model.
            } else {
                let b = (rng.next() % slots_n) as usize;
                let y = m.slots[b];
                if let Some(&first) = m.kids[y].first() {
                    m.slots[a] = first;
                }
            }
            if op % 512 == 511 || op == ops - 1 {
                let slots = m.slots.clone();
                m.walk(&slots, &mut digest);
            }
        }
        digest
    }
}

/// The collector the `stress_point` host function collects with, set while
/// a test runs (tests in this file run one at a time, see [`serial`]).
static COLLECTOR: AtomicPtr<GarbageCollector> = AtomicPtr::new(std::ptr::null_mut());
/// `stress_point` collects when a hash of its argument and this seed falls
/// below [`POINT_RATE`] (out of 2^16).
static POINT_SEED: AtomicU64 = AtomicU64::new(0);
static POINT_RATE: AtomicU64 = AtomicU64::new(0);
static POINT_COLLECTIONS: AtomicUsize = AtomicUsize::new(0);

/// Called by Codira code after every operation: sometimes collects, from
/// inside the running program (the calling thread parks itself first, so
/// its own frames are scanned like any other thread's).
extern "C" fn stress_point(op: i64) -> i64 {
    // The splitmix64 finalizer.
    let mut h = (op as u64) ^ POINT_SEED.load(Ordering::Relaxed).rotate_left(32);
    h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    if (h & 0xFFFF) < POINT_RATE.load(Ordering::Relaxed) {
        let gc = COLLECTOR.load(Ordering::Acquire);
        if !gc.is_null() {
            // Safety: set to the runtime's collector, which outlives the
            // call into Codira code that called this.
            unsafe { &*gc }.collect_blocking().unwrap();
            POINT_COLLECTIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
    0
}

/// Tests in this binary share the host function's statics and stop each
/// other's worlds; run them one at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn budget() -> Duration {
    let secs = std::env::var("CODIRA_STRESS_SECS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(3.0);
    Duration::from_secs_f64(secs)
}

fn first_seed() -> i64 {
    // Fixed by default, so a plain `cargo test` starts from the same graph
    // everywhere; the gate runner sets a fresh one.
    std::env::var("CODIRA_STRESS_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
}

fn build(
    configure: impl FnOnce(codira_runtime::RuntimeBuilder) -> codira_runtime::RuntimeBuilder,
) -> CompileAndRunTestDriver {
    CompileAndRunTestDriver::new(GRAPH, |builder| {
        configure(builder.insert_fn("stress_point", stress_point as extern "C" fn(i64) -> i64))
    })
    .expect("Failed to build test driver")
}

/// Sets the flag when dropped, so a collecting thread stops even when the
/// test fails.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Clears the collector pointer when the test ends.
struct ClearCollector;

impl Drop for ClearCollector {
    fn drop(&mut self) {
        COLLECTOR.store(std::ptr::null_mut(), Ordering::Release);
        POINT_RATE.store(0, Ordering::Relaxed);
    }
}

/// A host thread that collects at seeded random intervals until `done`.
fn host_collector<'s>(
    s: &'s std::thread::Scope<'s, '_>,
    gc: &'s GarbageCollector,
    done: &'s AtomicBool,
    seed: i64,
    pause_unit: Duration,
    count: &'s AtomicUsize,
) {
    s.spawn(move || {
        let mut rng = Rng(seed);
        while !done.load(Ordering::Relaxed) {
            // 0 to 63 units, sometimes back to back.
            let pause = (rng.next() % 64) as u32;
            if pause > 0 {
                std::thread::sleep(pause_unit * pause);
            }
            gc.collect_blocking().unwrap();
            count.fetch_add(1, Ordering::Relaxed);
        }
    });
}

/// Runs graphs of random seeds for the time budget, with collections from
/// inside the program (`point_rate`/65536 of operations) and from a host
/// thread (pausing up to 63 `host_pause_unit`s between them). With
/// `until_limit`, it also goes on until the heap limit has collected.
/// Returns the number of runs.
fn run_graphs(
    driver: &CompileAndRunTestDriver,
    until_limit: bool,
    point_rate: u64,
    host_pause_unit: Option<Duration>,
    tasks: i64,
    slots_n: i64,
    ops: i64,
) -> usize {
    let gc = driver.runtime.gc();
    COLLECTOR.store(std::ptr::from_ref(gc).cast_mut(), Ordering::Release);
    let _clear = ClearCollector;
    POINT_RATE.store(point_rate, Ordering::Relaxed);
    POINT_COLLECTIONS.store(0, Ordering::Relaxed);
    let first = first_seed();
    let seed = first;
    let deadline = Instant::now() + budget();
    let done = AtomicBool::new(false);
    let host_collections = AtomicUsize::new(0);
    let mut runs = 0;
    std::thread::scope(|s| {
        if let Some(unit) = host_pause_unit {
            host_collector(s, gc, &done, seed, unit, &host_collections);
        }
        let _done = SetOnDrop(&done);
        let mut seed = seed;
        // With `until_limit`, past the deadline it goes on (up to 500
        // graphs) until the limit has collected at least once: on a slow
        // machine (a sanitizer build) the time alone may not get there.
        let limit_unreached = || until_limit && gc.emergency_collections() == 0;
        while runs == 0 || Instant::now() < deadline || (limit_unreached() && runs < 500) {
            POINT_SEED.store(seed as u64, Ordering::Relaxed);
            let got: i64 = if tasks == 0 {
                driver
                    .runtime
                    .invoke("stress", (seed, slots_n, ops))
                    .unwrap()
            } else {
                driver
                    .runtime
                    .invoke("stress_tasks", (seed, slots_n, ops, tasks))
                    .unwrap()
            };
            assert!(got >= 0, "seed {seed}: the program saw {} bad nodes", -got);
            let expected = if tasks == 0 {
                Shadow::stress(seed, slots_n, ops)
            } else {
                (0..=tasks).fold(0, |sum, t| {
                    (sum + Shadow::stress(seed + t, slots_n, ops)) % MODULUS
                })
            };
            assert_eq!(
                got, expected,
                "seed {seed}: the graph differs from the shadow model"
            );
            seed += tasks + 1;
            runs += 1;
        }
    });
    eprintln!(
        "gc stress: {runs} runs from seed {first}, {} collections inside Codira code, {} from the host thread, {} at the heap limit",
        POINT_COLLECTIONS.load(Ordering::Relaxed),
        host_collections.load(Ordering::Relaxed),
        gc.emergency_collections(),
    );
    runs
}

/// After a run nothing is rooted and no thread runs Codira code, so a
/// collection must free everything.
fn assert_no_leak(driver: &CompileAndRunTestDriver) {
    let gc = driver.runtime.gc();
    gc.collect_blocking().unwrap();
    let left = GcRuntime::stats(gc).allocated_memory;
    assert_eq!(
        left, 0,
        "{left} bytes survived a collection with nothing rooted"
    );
}

#[test]
fn shadow_model_matches_itself_across_seeds() {
    // The shadow is deterministic; different seeds give different graphs.
    assert_eq!(Shadow::stress(7, 16, 2000), Shadow::stress(7, 16, 2000));
    assert_ne!(Shadow::stress(7, 16, 2000), Shadow::stress(8, 16, 2000));
}

#[test]
fn codira_graphs_survive_random_collections() {
    let _serial = serial();
    let driver = build(|builder| builder);
    run_graphs(
        &driver,
        false,
        64,
        Some(Duration::from_micros(13)),
        0,
        24,
        3_000,
    );
    assert_no_leak(&driver);
}

#[test]
fn codira_graphs_survive_collections_at_a_heap_limit() {
    let _serial = serial();
    let driver = build(|builder| builder.heap_limit(8 * 64 * 1024));
    // Rare host collections, so the limit is what collects most.
    run_graphs(
        &driver,
        true,
        16,
        Some(Duration::from_micros(400)),
        0,
        24,
        3_000,
    );
    let gc = driver.runtime.gc();
    assert!(
        gc.emergency_collections() > 0,
        "the limit was never reached"
    );
    assert!(gc.committed_bytes() <= 8 * 64 * 1024);
    assert_no_leak(&driver);
}

/// The bound pacing must keep a graph run's heap under: the live graph is
/// a few hundred KiB, pacing collects from 4 MiB of payload at 100%
/// growth, and size classes and block granularity add slack.
const PACED_PEAK_BOUND: usize = 32 * 1024 * 1024;

/// Nothing but pacing collects -- no heap limit, no host thread, no
/// collections from the program -- and the heap stays bounded while
/// megabytes of garbage go through it.
#[test]
fn pacing_alone_keeps_codira_graphs_bounded() {
    let _serial = serial();
    let driver = build(|builder| builder);
    run_graphs(&driver, false, 0, None, 0, 24, 3_000);
    let stats = driver.runtime.gc().heap_stats();
    eprintln!(
        "gc stress (pacing): peak heap {} bytes, {} bytes allocated, {} paced collections",
        stats.peak_heap_bytes, stats.bytes_allocated, stats.collections.paced
    );
    assert!(stats.collections.paced > 0, "pacing never collected");
    assert!(
        stats.peak_heap_bytes <= PACED_PEAK_BOUND,
        "the heap peaked at {} bytes",
        stats.peak_heap_bytes
    );
    assert_no_leak(&driver);
}

#[test]
fn graphs_on_worker_threads_survive_random_collections() {
    let _serial = serial();
    let driver = build(|builder| builder.heap_limit(48 * 64 * 1024));
    run_graphs(
        &driver,
        false,
        32,
        Some(Duration::from_micros(13)),
        3,
        16,
        2_000,
    );
    assert_no_leak(&driver);
}

/// The same graph operations driven from the host: the host keeps the slot
/// table as rooted handles and mutates the graph through calls into Codira
/// code and struct field reads, while a host thread collects.
#[test]
fn host_built_graphs_survive_random_collections() {
    let _serial = serial();
    let driver = build(|builder| builder);
    let runtime = &driver.runtime;
    let gc = runtime.gc();
    let seed = first_seed();
    let deadline = Instant::now() + budget();
    let done = AtomicBool::new(false);
    let host_collections = AtomicUsize::new(0);
    let mut runs = 0usize;
    std::thread::scope(|s| {
        host_collector(
            s,
            gc,
            &done,
            seed ^ 0x55,
            Duration::from_micros(13),
            &host_collections,
        );
        let _done = SetOnDrop(&done);
        let mut seed = seed;
        while runs == 0 || Instant::now() < deadline {
            host_graph(runtime, seed, 12, 600);
            seed += 1;
            runs += 1;
        }
    });
    eprintln!(
        "gc stress (host API): {runs} runs, {} host collections",
        host_collections.load(Ordering::Relaxed)
    );
    assert_no_leak(&driver);
}

/// One host-driven graph run, checked against a shadow model.
fn host_graph(runtime: &codira_runtime::Runtime, seed: i64, slots_n: usize, ops: usize) {
    let mut rng = Rng(seed);
    let mut m = Shadow::new();
    let mut slots: Vec<RootedStruct> = Vec::new();
    let mut next_id = 0i64;
    let new_node = |id: i64| -> RootedStruct {
        // Unrooted between the return and `root`: hold a guard so a
        // collection on the other thread scans this stack meanwhile.
        let _guard = codira_runtime::enter_codira();
        let node: StructRef<'_> = runtime.invoke("new_node", (id,)).unwrap();
        node.root()
    };
    for _ in 0..slots_n {
        slots.push(new_node(next_id));
        let n = m.node(next_id);
        m.slots.push(n);
        next_id += 1;
    }
    for op in 0..ops {
        let r = rng.next() % 100;
        let a = (rng.next() as usize) % slots_n;
        if r < 35 {
            slots[a] = new_node(next_id);
            m.slots[a] = m.node(next_id);
            next_id += 1;
        } else if r < 65 {
            let b = (rng.next() as usize) % slots_n;
            let at = rng.next() % 4;
            let (x, y) = (m.slots[a], m.slots[b]);
            if m.kids[x].len() < 4 {
                m.kids[x].push(y);
            } else {
                m.kids[x][at as usize] = y;
            }
            let () = runtime
                .invoke(
                    "link",
                    (slots[a].as_ref(runtime), slots[b].as_ref(runtime), at),
                )
                .unwrap();
        } else if r < 75 {
            let x = m.slots[a];
            m.kids[x].pop();
            let bad: i64 = runtime
                .invoke("unlink", (slots[a].as_ref(runtime),))
                .unwrap();
            assert_eq!(bad, 0, "seed {seed} op {op}: an unlinked node was corrupt");
        } else if r < 90 {
            let node = slots[a].as_ref(runtime);
            let id: i64 = node.get("id").unwrap();
            let check: i64 = node.get("check").unwrap();
            assert_eq!(id, m.ids[m.slots[a]], "seed {seed} op {op}");
            assert_eq!(check, check_of(id), "seed {seed} op {op}");
            let count: i64 = runtime
                .invoke("kid_count", (slots[a].as_ref(runtime),))
                .unwrap();
            assert_eq!(
                count as usize,
                m.kids[m.slots[a]].len(),
                "seed {seed} op {op}"
            );
        } else {
            let b = (rng.next() as usize) % slots_n;
            let y = m.slots[b];
            if let Some(&first) = m.kids[y].first() {
                m.slots[a] = first;
                let _guard = codira_runtime::enter_codira();
                let kid: StructRef<'_> = runtime
                    .invoke("first_kid", (slots[b].as_ref(runtime),))
                    .unwrap();
                slots[a] = kid.root();
            }
        }
        if op % 100 == 99 {
            let a = (rng.next() as usize) % slots_n;
            let got: i64 = runtime
                .invoke(
                    "walk_from",
                    (slots[a].as_ref(runtime), (op + 1) as i64 + seed * 1000),
                )
                .unwrap();
            let mut expected = 0;
            m.walk(&[m.slots[a]], &mut expected);
            assert_eq!(
                got, expected,
                "seed {seed} op {op}: the graph differs from the shadow"
            );
        }
    }
}

/// Host threads allocate through the host API while Codira code on another
/// thread runs graphs and collects by pacing (set low, so collections are
/// frequent). Each host thread holds some objects unrooted on its stack
/// inside a mutator guard -- which a collection must find by scanning that
/// stack -- and others rooted, used with no guard; every object must keep
/// what was written into it. (Without a guard, an object between its
/// allocation and its root is unprotected from a collection on another
/// thread: the runtime documents that the host takes a guard there.) This is the shape of the use-after-free in which a
/// paced collection started by one host thread freed another host thread's
/// unrooted object.
#[test]
fn host_threads_allocate_while_codira_code_collects() {
    let _serial = serial();
    let driver = build(|builder| {
        builder.gc_policy(codira_runtime::GcPolicy {
            growth_percent: Some(25),
            min_heap: 256 * 1024,
            ..codira_runtime::GcPolicy::default()
        })
    });
    let runtime = &driver.runtime;
    let gc = runtime.gc();
    let ty = runtime.get_type_info_by_name("GNode").expect("GNode");
    let fields = ty.as_struct().unwrap().fields();
    let id_at = fields.find_by_name("id").unwrap().offset();
    let check_at = fields.find_by_name("check").unwrap().offset();
    let deadline = Instant::now() + budget();
    let done = AtomicBool::new(false);
    let checked = AtomicUsize::new(0);

    let write = |node: GcPtr, id: i64| unsafe {
        let mut node = node;
        let base = node.deref_mut::<u8>();
        base.add(id_at).cast::<i64>().write(id);
        base.add(check_at).cast::<i64>().write(check_of(id));
    };
    let read = |node: GcPtr| unsafe {
        let base = node.deref::<u8>();
        (
            base.add(id_at).cast::<i64>().read(),
            base.add(check_at).cast::<i64>().read(),
        )
    };

    std::thread::scope(|s| {
        for t in 0..3i64 {
            let (ty, done, checked) = (&ty, &done, &checked);
            s.spawn(move || {
                let mut round = 0i64;
                while !done.load(Ordering::Relaxed) {
                    // Unrooted, on this stack, inside a guard.
                    {
                        let _guard = codira_runtime::enter_codira();
                        let mut held = [GcPtr::from(std::ptr::null::<*mut std::ffi::c_void>()); 16];
                        for (k, slot) in held.iter_mut().enumerate() {
                            *slot = gc.try_alloc(ty).expect("allocation");
                            write(*slot, t << 40 | round << 8 | k as i64);
                        }
                        for _ in 0..64 {
                            let _garbage = gc.try_alloc(ty).expect("allocation");
                        }
                        let held = std::hint::black_box(held);
                        for (k, &node) in held.iter().enumerate() {
                            let id = t << 40 | round << 8 | k as i64;
                            assert_eq!(
                                read(node),
                                (id, check_of(id)),
                                "an unrooted object held in a guard changed"
                            );
                        }
                    }
                    // Rooted, with no guard.
                    // Rooted inside a brief guard (between the allocation
                    // and the root, a collection that Codira code starts on
                    // another thread must scan this stack), then used with
                    // no guard.
                    let rooted: Vec<GcPtr> = (0..16i64)
                        .map(|k| {
                            let _guard = codira_runtime::enter_codira();
                            let node = gc.try_alloc(ty).expect("allocation");
                            gc.root(node);
                            write(node, -(t << 40 | round << 8 | k));
                            node
                        })
                        .collect();
                    std::thread::yield_now();
                    for (k, &node) in rooted.iter().enumerate() {
                        let id = -(t << 40 | round << 8 | k as i64);
                        assert_eq!(read(node), (id, check_of(id)), "a rooted object changed");
                        gc.unroot(node);
                    }
                    checked.fetch_add(32, Ordering::Relaxed);
                    round += 1;
                }
            });
        }
        let _done = SetOnDrop(&done);
        let mut seed = first_seed();
        while Instant::now() < deadline {
            let got: i64 = runtime.invoke("stress", (seed, 24i64, 2_000i64)).unwrap();
            assert_eq!(got, Shadow::stress(seed, 24, 2_000), "seed {seed}");
            seed += 1;
        }
    });
    let stats = gc.heap_stats();
    eprintln!(
        "gc stress (host threads): {} host objects checked, {} paced collections",
        checked.load(Ordering::Relaxed),
        stats.collections.paced
    );
    assert!(stats.collections.paced > 0, "pacing never collected");
    assert_no_leak(&driver);
}
