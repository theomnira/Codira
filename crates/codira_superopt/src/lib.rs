//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! `codira_superopt` -- bounded, verified superoptimisation of straight-line
//! integer machine code (experimental: nothing in `codira build` calls it yet).
//!
//! Given a [`Block`] with one live-out register, the search looks for a
//! cheaper instruction sequence computing the same value, under a target
//! [`Profile`]:
//!
//! 1. **Semantics of the original.** The block is run on test vectors with
//!    the concrete semantics of `codira_mcode`.
//! 2. **Candidate generation.** Bottom-up enumeration of expression trees
//!    whose nodes are *instruction templates* of the target (each a real
//!    instruction with placeholder registers and a chosen immediate), up to
//!    `max_insts` nodes. Trees that compute the same values on every test
//!    vector are merged, keeping a small Pareto set by (latency, micro-ops)
//!    -- "observational equivalence" pruning from enumerative program
//!    synthesis.
//! 3. **Reject invalid candidates.** A tree that matches the original on
//!    all test vectors is lowered to registers (two-address forms get a
//!    `mov` when their first operand is still needed; only registers the
//!    original writes or its live-out may be overwritten) and the *lowered
//!    machine code* is proven equivalent to the original with Z3
//!    ([`codira_mcode::prove_equivalent`]). A counterexample becomes a new
//!    test vector and the search is repeated (counterexample-guided
//!    inductive synthesis).
//! 4. **Dependencies.** 5. **Target cost.** Every verified candidate is
//!    costed with the profile (critical path, port pressure, issue width,
//!    size).
//! 6. **Measurement** (optional, [`select_by_measurement`]): the best
//!    candidates and the original are run on the host processor and the
//!    fastest measured one is chosen.
//! 7. **Independent validation.** The chosen sequence's equivalence proof is
//!    stored as a self-contained SMT-LIB2 query; [`Decision::revalidate`]
//!    re-proves it, and a cache hit is always revalidated before use.
//! 9. **Provenance.** A [`Decision`] records the original, the result, the
//!    profile identity, the costs, the evidence and the search statistics.
//!
//! Step 8 (debug and unwind information) does not arise: blocks contain no
//! calls, stack operations or frame setup, and a rewrite keeps the block's
//! boundaries.
//!
//! Limits (refused, not approximated): code that must run in constant time,
//! or whose status is unknown ([`Origin`]); one live-out; flags dead at the end
//! of the block (all modelled flag-writing instructions overwrite them); no
//! memory; the original's registers that it does not write are preserved.

pub mod library;

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use codira_mcode::aarch64::{self, Aarch64, BinOp, ShiftKind};
use codira_mcode::measure::{Harness, Measurement, Mode};
use codira_mcode::profile::{Cost, Objective, Profile};
use codira_mcode::x86::{self, AluOp, ShiftOp, X86_64};
use codira_mcode::{prove_equivalent, Block, Concrete, Equivalence, Isa};

/// An instruction set the search can generate code for.
pub trait SynthIsa: Isa {
    /// Placeholder registers for template arguments 0, 1, 2 and for the
    /// destination of untied templates. They must be distinct.
    fn placeholders() -> [Self::Reg; 4];
    fn mov(dst: Self::Reg, src: Self::Reg) -> Self::Inst;
    /// Instruction templates, given the immediate pool (constants of the
    /// original block plus small values) and the profile's features.
    fn templates(pool: &[i64], features: &[String]) -> Vec<Template<Self>>;
}

/// A template: an instruction over placeholder registers.
#[derive(Clone, Debug)]
pub struct Template<I: Isa> {
    pub inst: I::Inst,
    pub arity: usize,
    /// Whether the destination is argument 0's register (two-address).
    pub tied: bool,
}

impl<I: SynthIsa> Template<I> {
    fn new(inst: I::Inst) -> Template<I> {
        let [p0, p1, p2, _] = I::placeholders();
        let reads = I::reads(&inst);
        let arity = [p0, p1, p2].iter().filter(|p| reads.contains(p)).count();
        let tied = I::writes(&inst) == p0;
        Template { inst, arity, tied }
    }

    fn eval(&self, args: &[u64]) -> u64 {
        let [p0, p1, p2, _] = I::placeholders();
        let read = |r: I::Reg| -> u64 {
            if r == p0 {
                args[0]
            } else if r == p1 {
                args[1]
            } else if r == p2 {
                args[2]
            } else {
                panic!("template reads a non-placeholder register")
            }
        };
        I::eval(&Concrete, &self.inst, &read)
    }
}

/// Search configuration. Part of the cache key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Config {
    pub max_insts: usize,
    pub objective: Objective,
    /// Upper bound on enumerated trees (the search space actually explored
    /// is reported in [`Stats`]).
    pub max_terms: usize,
    /// Verified candidates kept for selection.
    pub keep: usize,
    /// Counterexample-guided rounds before giving up.
    pub max_rounds: usize,
    pub time_limit_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_insts: 3,
            objective: Objective::Latency,
            max_terms: 400_000,
            keep: 4,
            max_rounds: 8,
            time_limit_ms: 20_000,
        }
    }
}

/// Search statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub templates: usize,
    /// Trees whose values were computed (the explored search space).
    pub enumerated: u64,
    /// Distinct value signatures kept in the bank.
    pub kept: usize,
    /// Trees that matched the original on every test vector.
    pub matches: u64,
    pub smt_queries: u64,
    pub counterexamples: u64,
    pub test_vectors: usize,
    pub rounds: usize,
    pub elapsed_ms: u64,
    /// The search stopped at `max_terms` or the time limit, so it was not
    /// exhaustive up to `max_insts`.
    pub truncated: bool,
}

/// A verified candidate.
#[derive(Clone, Debug)]
pub struct Candidate<I: Isa> {
    pub block: Block<I>,
    pub cost: Cost,
    /// SMT-LIB2 query whose `unsat` answer proves equivalence.
    pub evidence: String,
}

/// The result of superoptimising one block.
#[derive(Clone, Debug)]
pub struct Decision<I: Isa> {
    pub original: Block<I>,
    pub original_cost: Cost,
    pub profile: String,
    pub objective: Objective,
    /// Verified candidates, best first under the profile. The first one is
    /// the choice when it beats the original.
    pub candidates: Vec<Candidate<I>>,
    pub stats: Stats,
    pub cache_key: String,
}

impl<I: SynthIsa> Decision<I> {
    /// The chosen replacement, if one is strictly better under the profile.
    pub fn improvement(&self) -> Option<&Candidate<I>> {
        self.candidates
            .first()
            .filter(|c| c.cost.key(self.objective) < self.original_cost.key(self.objective))
    }

    /// Re-proves every candidate against the original.
    pub fn revalidate(&self) -> bool {
        self.candidates
            .iter()
            .all(|c| prove_equivalent(&self.original, &c.block).is_proven())
    }
}

impl<I: SynthIsa> fmt::Display for Decision<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "profile   {}", self.profile)?;
        writeln!(f, "objective {:?}", self.objective)?;
        writeln!(f, "key       {}", self.cache_key)?;
        writeln!(f, "original  ({})", self.original_cost)?;
        for i in &self.original.insts {
            writeln!(f, "    {i}")?;
        }
        match self.improvement() {
            Some(c) => {
                writeln!(f, "chosen    ({}) -- proven equivalent by Z3", c.cost)?;
                for i in &c.block.insts {
                    writeln!(f, "    {i}")?;
                }
            }
            None => writeln!(
                f,
                "chosen    original (no verified candidate is cheaper under this profile)"
            )?,
        }
        let s = &self.stats;
        writeln!(
            f,
            "search    {} templates, {} trees enumerated, {} signatures kept, {} matches, {} SMT queries, \
             {} counterexamples, {} test vectors, {} rounds, {} ms{}",
            s.templates,
            s.enumerated,
            s.kept,
            s.matches,
            s.smt_queries,
            s.counterexamples,
            s.test_vectors,
            s.rounds,
            s.elapsed_ms,
            if s.truncated { ", truncated" } else { "" }
        )
    }
}

// ---------------------------------------------------------------------------
// Enumeration

#[derive(Clone, Debug)]
struct Term {
    /// `usize::MAX - i` for live-in `i`; otherwise a template index.
    op: usize,
    args: [u32; 3],
    size: u32,
    latency: u32,
    uops: u32,
}

const LEAF: usize = usize::MAX;

struct Bank {
    terms: Vec<Term>,
    sigs: Vec<Vec<u64>>,
    by_sig: HashMap<Vec<u64>, Vec<u32>>,
    by_size: Vec<Vec<u32>>,
}

const PARETO: usize = 2;

impl Bank {
    /// Adds a term unless an existing term with the same signature is at
    /// least as good in both latency and micro-ops (or the Pareto set is
    /// full and the term is not better than its worst member).
    fn insert(&mut self, term: Term, sig: Vec<u64>) -> bool {
        let entry = self.by_sig.entry(sig.clone()).or_default();
        for &id in entry.iter() {
            let t = &self.terms[id as usize];
            if t.latency <= term.latency && t.uops <= term.uops {
                return false;
            }
        }
        if entry.len() >= PARETO {
            return false;
        }
        let id = self.terms.len() as u32;
        entry.push(id);
        let size = term.size as usize;
        self.terms.push(term);
        self.sigs.push(sig);
        if self.by_size.len() <= size {
            self.by_size.resize(size + 1, Vec::new());
        }
        self.by_size[size].push(id);
        true
    }
}

fn test_vectors(n_inputs: usize, extra: &[Vec<u64>]) -> Vec<Vec<u64>> {
    let corners = [
        0u64,
        1,
        u64::MAX,
        1 << 63,
        2,
        0x7fff_ffff_ffff_ffff,
        3,
        0x8000_0000,
    ];
    let mut out = Vec::new();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for c in corners {
        out.push(
            (0..n_inputs)
                .map(|i| if i == 0 { c } else { next() })
                .collect(),
        );
    }
    for _ in 0..8 {
        out.push((0..n_inputs).map(|_| next()).collect());
    }
    out.extend(extra.iter().cloned());
    out
}

fn immediate_pool<I: Isa>(block: &Block<I>) -> Vec<i64> {
    // Immediates of the original, found by evaluating each instruction with
    // its register inputs at zero is not reliable; instead parse them from
    // the printed form, which every model prints in decimal.
    let mut pool: Vec<i64> = vec![0, 1, 2, 3, 4, 5, 8, 9, 16, 32, 63, -1];
    for inst in &block.insts {
        let text = inst.to_string();
        for tok in text.split(|c: char| !(c.is_ascii_digit() || c == '-')) {
            if let Ok(v) = tok.parse::<i64>() {
                if tok.starts_with(|c: char| c.is_ascii_digit() || c == '-') && !tok.is_empty() {
                    pool.extend([v, v.wrapping_neg(), v.wrapping_sub(1), v.wrapping_add(1)]);
                }
            }
        }
    }
    pool.sort_unstable();
    pool.dedup();
    pool
}

/// Whether the code a block comes from must run in constant time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConstantTime {
    /// The function is not `oblivious` (or otherwise constant-time), and
    /// the block is reachable from at least one function that is not.
    NotRequired,
    /// The function is `oblivious`, or the block is reachable only from
    /// constant-time functions.
    Required,
    /// The caller cannot tell (for example, an object file without the
    /// source's annotations).
    Unknown,
}

/// Where a block comes from. Every entry point takes one, so that code with
/// timing obligations is refused explicitly rather than rewritten.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Origin {
    pub function: String,
    pub constant_time: ConstantTime,
}

impl Origin {
    pub fn new(function: impl Into<String>, constant_time: ConstantTime) -> Origin {
        Origin {
            function: function.into(),
            constant_time,
        }
    }

    /// Refuses code whose timing must not change, or whose status is
    /// unknown. A rewrite proven equivalent in value can still change which
    /// instructions run and how long they take (an `imul` replaced by a
    /// data-independent shift sequence is harmless; the reverse direction,
    /// or a `cmov` turned into arithmetic, need not be), and no machine-code
    /// constant-time check rechecks the result yet.
    pub fn check(&self) -> Result<(), String> {
        match self.constant_time {
            ConstantTime::NotRequired => Ok(()),
            ConstantTime::Required => Err(format!(
                "refusing to superoptimise `{}`: it must run in constant time, a rewrite may change its \
                 timing, and no machine-code constant-time check can recheck the result yet",
                self.function
            )),
            ConstantTime::Unknown => Err(format!(
                "refusing to superoptimise `{}`: whether it must run in constant time is unknown \
                 (pass `ConstantTime::NotRequired` only when the source says so)",
                self.function
            )),
        }
    }
}

/// Superoptimises `block`, which comes from `origin`, under `profile`.
/// Refuses constant-time and unknown origins ([`Origin::check`]).
pub fn superoptimize<I: SynthIsa>(
    origin: &Origin,
    block: &Block<I>,
    profile: &Profile,
    config: &Config,
) -> Result<Decision<I>, String> {
    origin.check()?;
    block.check()?;
    profile.supports(block)?;
    if block.live_out.len() != 1 {
        return Err(format!(
            "{} live-out registers; this version superoptimises single-result blocks only",
            block.live_out.len()
        ));
    }
    let original_cost = profile.cost(block)?;
    let start = Instant::now();
    let deadline = start + Duration::from_millis(config.time_limit_ms);
    let pool = immediate_pool(block);
    let templates = I::templates(&pool, &profile.features)
        .into_iter()
        .filter(|t| profile.forms.contains_key(I::form(&t.inst)))
        .collect::<Vec<_>>();
    let mut stats = Stats {
        templates: templates.len(),
        ..Stats::default()
    };
    let mut extra: Vec<Vec<u64>> = Vec::new();
    let mut verified: Vec<Candidate<I>> = Vec::new();
    for round in 0..config.max_rounds {
        stats.rounds = round + 1;
        let tests = test_vectors(block.live_in.len(), &extra);
        stats.test_vectors = tests.len();
        let goal: Vec<u64> = tests.iter().map(|v| block.run(v)[0]).collect();
        let matches = enumerate(
            block, profile, config, &templates, &tests, &goal, deadline, &mut stats,
        );
        // Lower, cost, sort, verify.
        let mut lowered: Vec<(Block<I>, Cost)> = Vec::new();
        for (bank, id) in &matches {
            if let Some(b) = lower(bank, *id, &templates, block) {
                if let Ok(c) = profile.cost(&b) {
                    if !lowered.iter().any(|(x, _)| x == &b) {
                        lowered.push((b, c));
                    }
                }
            }
        }
        lowered.sort_by_key(|(_, c)| c.key(config.objective));
        let mut refuted = false;
        for (b, c) in lowered {
            if verified.len() >= config.keep {
                break;
            }
            stats.smt_queries += 1;
            match prove_equivalent(block, &b) {
                Equivalence::Proven { smtlib } => verified.push(Candidate {
                    block: b,
                    cost: c,
                    evidence: smtlib,
                }),
                Equivalence::Refuted { inputs } => {
                    stats.counterexamples += 1;
                    extra.push(inputs);
                    refuted = true;
                    break;
                }
                Equivalence::Unknown { .. } | Equivalence::Incomparable { .. } => {}
            }
        }
        if !refuted || Instant::now() > deadline {
            break;
        }
        verified.clear();
    }
    verified.sort_by_key(|c| c.cost.key(config.objective));
    stats.elapsed_ms = start.elapsed().as_millis() as u64;
    Ok(Decision {
        cache_key: cache_key(block, profile, config),
        original: block.clone(),
        original_cost,
        profile: profile.id.clone(),
        objective: config.objective,
        candidates: verified,
        stats,
    })
}

#[allow(clippy::too_many_arguments)]
fn enumerate<I: SynthIsa>(
    block: &Block<I>,
    profile: &Profile,
    config: &Config,
    templates: &[Template<I>],
    tests: &[Vec<u64>],
    goal: &[u64],
    deadline: Instant,
    stats: &mut Stats,
) -> Vec<(std::rc::Rc<Bank>, u32)> {
    let mut bank = Bank {
        terms: Vec::new(),
        sigs: Vec::new(),
        by_sig: HashMap::new(),
        by_size: vec![Vec::new()],
    };
    let mut goals: Vec<Term> = Vec::new();
    let mut goal_sigs = 0;
    for i in 0..block.live_in.len() {
        let sig: Vec<u64> = tests.iter().map(|v| v[i]).collect();
        let t = Term {
            op: LEAF - i,
            args: [0; 3],
            size: 0,
            latency: 0,
            uops: 0,
        };
        if sig == goal {
            goals.push(t.clone());
        }
        bank.insert(t, sig);
    }
    let lat = |t: &Template<I>| -> (u32, u32) {
        let c = profile.form_cost::<I>(&t.inst).expect("filtered");
        ((c.latency * 100.0).round() as u32, c.uops.max(1))
    };
    let costs: Vec<(u32, u32)> = templates.iter().map(lat).collect();
    'outer: for size in 1..=config.max_insts as u32 {
        let mut new_terms: Vec<(Term, Vec<u64>)> = Vec::new();
        for (ti, t) in templates.iter().enumerate() {
            // Distribute size-1 among the arguments.
            let rest = size - 1;
            let splits: Vec<[u32; 3]> = match t.arity {
                0 => {
                    if rest == 0 {
                        vec![[0, 0, 0]]
                    } else {
                        vec![]
                    }
                }
                1 => vec![[rest, 0, 0]],
                2 => (0..=rest).map(|a| [a, rest - a, 0]).collect(),
                _ => (0..=rest)
                    .flat_map(|a| (0..=rest - a).map(move |b| [a, b, rest - a - b]))
                    .collect(),
            };
            for split in splits {
                let lists: Vec<&[u32]> = (0..t.arity)
                    .map(|k| {
                        bank.by_size
                            .get(split[k] as usize)
                            .map_or(&[][..], std::vec::Vec::as_slice)
                    })
                    .collect();
                if lists.iter().any(|l| l.is_empty()) && t.arity > 0 {
                    continue;
                }
                let mut idx = [0usize; 3];
                loop {
                    if stats.enumerated as usize >= config.max_terms || Instant::now() > deadline {
                        stats.truncated = true;
                        break 'outer;
                    }
                    let args: [u32; 3] = [
                        if t.arity > 0 { lists[0][idx[0]] } else { 0 },
                        if t.arity > 1 { lists[1][idx[1]] } else { 0 },
                        if t.arity > 2 { lists[2][idx[2]] } else { 0 },
                    ];
                    stats.enumerated += 1;
                    let sig: Vec<u64> = (0..tests.len())
                        .map(|j| {
                            let a = [
                                bank.sigs[args[0] as usize].get(j).copied().unwrap_or(0),
                                if t.arity > 1 {
                                    bank.sigs[args[1] as usize][j]
                                } else {
                                    0
                                },
                                if t.arity > 2 {
                                    bank.sigs[args[2] as usize][j]
                                } else {
                                    0
                                },
                            ];
                            t.eval(&a)
                        })
                        .collect();
                    let (l, u) = costs[ti];
                    let arg_lat = (0..t.arity)
                        .map(|k| bank.terms[args[k] as usize].latency)
                        .max()
                        .unwrap_or(0);
                    let arg_uops: u32 = (0..t.arity)
                        .map(|k| bank.terms[args[k] as usize].uops)
                        .sum();
                    let term = Term {
                        op: ti,
                        args,
                        size,
                        latency: arg_lat + l,
                        uops: arg_uops + u,
                    };
                    if sig == goal {
                        goal_sigs += 1;
                        goals.push(term);
                    } else {
                        new_terms.push((term, sig));
                    }
                    // Advance the odometer.
                    let mut k = 0;
                    loop {
                        if k >= t.arity {
                            break;
                        }
                        idx[k] += 1;
                        if idx[k] < lists[k].len() {
                            break;
                        }
                        idx[k] = 0;
                        k += 1;
                    }
                    if k >= t.arity {
                        break;
                    }
                }
            }
        }
        for (t, s) in new_terms {
            bank.insert(t, s);
        }
    }
    stats.matches += goal_sigs + goals.iter().filter(|t| t.size == 0).count() as u64;
    stats.kept = bank.terms.len();
    // Goals reference bank terms as arguments: append them to the bank.
    let mut ids = Vec::new();
    for g in goals {
        ids.push(bank.terms.len() as u32);
        bank.terms.push(g);
        bank.sigs.push(Vec::new());
    }
    let bank = std::rc::Rc::new(bank);
    ids.into_iter().map(|id| (bank.clone(), id)).collect()
}

// ---------------------------------------------------------------------------
// Lowering

/// Lowers the expression DAG rooted at `root` to registers. Registers that
/// may be written: the live-out and every register the original block
/// writes; every other register (in particular live-ins the original leaves
/// alone) is preserved. Returns `None` when that pool is exhausted.
fn lower<I: SynthIsa>(
    bank: &Bank,
    root: u32,
    templates: &[Template<I>],
    original: &Block<I>,
) -> Option<Block<I>> {
    let out = original.live_out[0];
    let mut writable: Vec<I::Reg> = vec![out];
    for r in original.written() {
        if !writable.contains(&r) {
            writable.push(r);
        }
    }
    let is_leaf = |id: u32| bank.terms[id as usize].op > LEAF - 64;
    let args_of = |id: u32| -> Vec<u32> {
        let t = &bank.terms[id as usize];
        if t.op > LEAF - 64 {
            vec![]
        } else {
            t.args[..templates[t.op].arity].to_vec()
        }
    };
    // Post-order, each node once (a DAG: equal subterms are shared).
    let mut order: Vec<u32> = Vec::new();
    let mut stack: Vec<(u32, bool)> = vec![(root, false)];
    while let Some((id, expanded)) = stack.pop() {
        if order.contains(&id) {
            continue;
        }
        if expanded {
            order.push(id);
        } else {
            stack.push((id, true));
            for a in args_of(id).into_iter().rev() {
                stack.push((a, false));
            }
        }
    }
    let mut uses: HashMap<u32, usize> = HashMap::new();
    for &id in &order {
        for a in args_of(id) {
            *uses.entry(a).or_default() += 1;
        }
    }
    let mut loc: HashMap<u32, I::Reg> = HashMap::new();
    // Register -> node whose value it holds.
    let mut holder: HashMap<I::Reg, u32> = HashMap::new();
    for &id in &order {
        if is_leaf(id) {
            let r = original.live_in[LEAF - bank.terms[id as usize].op];
            loc.insert(id, r);
            holder.insert(r, id);
        }
    }
    let [p0, p1, p2, pd] = I::placeholders();
    let mut insts: Vec<I::Inst> = Vec::new();
    for &id in &order {
        if is_leaf(id) {
            continue;
        }
        let t = &bank.terms[id as usize];
        let tpl = &templates[t.op];
        let args = args_of(id);
        let regs: Vec<I::Reg> = args.iter().map(|a| loc[a]).collect();
        for a in &args {
            *uses.get_mut(a).unwrap() -= 1;
        }
        let is_root = id == root;
        let free = |r: &I::Reg, holder: &HashMap<I::Reg, u32>, uses: &HashMap<u32, usize>| {
            writable.contains(r)
                && holder
                    .get(r)
                    .is_none_or(|h| uses.get(h).copied().unwrap_or(0) == 0)
        };
        let pick =
            |exclude: &[I::Reg], holder: &HashMap<I::Reg, u32>, uses: &HashMap<u32, usize>| {
                let first = if is_root { Some(out) } else { None };
                first
                    .into_iter()
                    .chain(writable.iter().copied().filter(|r| *r != out))
                    .chain(std::iter::once(out))
                    .find(|r| !exclude.contains(r) && free(r, holder, uses))
            };
        let dst = if tpl.tied {
            let a0 = regs[0];
            if free(&a0, &holder, &uses) {
                a0
            } else {
                // Copy argument 0 first; the copy must not overwrite any
                // argument of this instruction.
                let d = pick(&regs, &holder, &uses)?;
                insts.push(I::mov(d, a0));
                d
            }
        } else {
            pick(&[], &holder, &uses)?
        };
        let map = |r: I::Reg| -> I::Reg {
            if r == p0 {
                if tpl.tied {
                    dst
                } else {
                    regs[0]
                }
            } else if r == p1 {
                regs[1]
            } else if r == p2 {
                regs[2]
            } else if r == pd {
                dst
            } else {
                r
            }
        };
        insts.push(I::rename(&tpl.inst, &map));
        loc.insert(id, dst);
        holder.insert(dst, id);
        uses.entry(id).or_insert(0);
    }
    let final_reg = loc[&root];
    if final_reg != out {
        insts.push(I::mov(out, final_reg));
    }
    let b = Block::new(insts, original.live_in.clone(), original.live_out.clone());
    // Structural guarantee the equivalence proof does not cover: nothing
    // outside the writable set is written.
    if b.check().is_err() || b.written().iter().any(|r| !writable.contains(r)) {
        return None;
    }
    Some(b)
}

// ---------------------------------------------------------------------------
// Measurement-based selection

/// Measures the original and up to `n` verified candidates on the host
/// (x86-64 only) and returns `(index, measurement)` for each, `None` being
/// the original. The fastest measured candidate is the choice; the caller
/// decides whether the measured difference exceeds the noise.
pub fn select_by_measurement(
    decision: &Decision<X86_64>,
    n: usize,
    mode: Mode,
    dir: &Path,
) -> Result<Vec<(Option<usize>, Measurement)>, String> {
    let h = Harness::detect(dir)?;
    let mut blocks = vec![decision.original.clone()];
    blocks.extend(decision.candidates.iter().take(n).map(|c| c.block.clone()));
    let m = h.measure(&blocks, mode)?;
    Ok(m.into_iter()
        .enumerate()
        .map(|(i, m)| (if i == 0 { None } else { Some(i - 1) }, m))
        .collect())
}

// ---------------------------------------------------------------------------
// Cache

/// FNV-1a 64-bit, stable across platforms and compiler versions.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The content address of a search: the ISA, the block with its live
/// registers, the complete profile text, the configuration and this
/// crate's version. Any change to any of them is a different key.
pub fn cache_key<I: Isa>(block: &Block<I>, profile: &Profile, config: &Config) -> String {
    let text = format!(
        "{}\n{block}{:?}\n{:?}\n{profile}{config:?}\n{}",
        I::NAME,
        block.live_in,
        block.live_out,
        env!("CARGO_PKG_VERSION")
    );
    format!("{:016x}", fnv1a(text.as_bytes()))
}

/// Looks up a previous decision's chosen block in `dir`, revalidates it
/// with Z3 against `block`, and returns it; or runs the search and stores
/// the result. The cache stores only the chosen sequence (or that the
/// original was kept); evidence is always recomputed, never trusted.
pub fn superoptimize_cached<I: SynthIsa>(
    origin: &Origin,
    block: &Block<I>,
    profile: &Profile,
    config: &Config,
    dir: &Path,
) -> Result<(Option<Block<I>>, bool), String> {
    origin.check()?;
    let key = cache_key(block, profile, config);
    let path = dir.join(format!("{key}.mcode"));
    if let Ok(text) = std::fs::read_to_string(&path) {
        if text.trim() == "original" {
            return Ok((None, true));
        }
        if let Ok(b) = Block::<I>::parse(&text, block.live_in.clone(), block.live_out.clone()) {
            if prove_equivalent(block, &b).is_proven() && profile.supports(&b).is_ok() {
                return Ok((Some(b), true));
            }
        }
        // Stale or corrupt entry: fall through and recompute.
    }
    let d = superoptimize(origin, block, profile, config)?;
    let chosen = d.improvement().map(|c| c.block.clone());
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let body = match &chosen {
        Some(b) => b.to_string(),
        None => "original\n".into(),
    };
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok((chosen, false))
}

// ---------------------------------------------------------------------------
// Targets

impl SynthIsa for X86_64 {
    fn placeholders() -> [x86::Reg; 4] {
        [x86::Reg::Rdi, x86::Reg::Rsi, x86::Reg::Rdx, x86::Reg::Rax]
    }

    fn mov(dst: x86::Reg, src: x86::Reg) -> x86::Inst {
        x86::Inst::Mov { dst, src }
    }

    fn templates(pool: &[i64], features: &[String]) -> Vec<Template<X86_64>> {
        use x86::Inst as X;
        let [a, b, _, d] = X86_64::placeholders();
        let mut v = Vec::new();
        let i32s: Vec<i32> = pool.iter().filter_map(|x| i32::try_from(*x).ok()).collect();
        for op in [
            AluOp::Add,
            AluOp::Sub,
            AluOp::And,
            AluOp::Or,
            AluOp::Xor,
            AluOp::Imul,
        ] {
            v.push(X::Alu { op, dst: a, src: b });
        }
        for op in [AluOp::Add, AluOp::Sub, AluOp::And, AluOp::Or, AluOp::Xor] {
            for &imm in &i32s {
                if imm != 0 {
                    v.push(X::AluImm { op, dst: a, imm });
                }
            }
        }
        for &imm in &i32s {
            if !(-1..=1).contains(&imm) {
                v.push(X::Imul3 {
                    dst: d,
                    src: a,
                    imm,
                });
            }
        }
        for op in [ShiftOp::Shl, ShiftOp::Shr, ShiftOp::Sar] {
            for amount in 1..64u8 {
                v.push(X::Shift { op, dst: a, amount });
            }
        }
        v.push(X::Neg { dst: a });
        v.push(X::Not { dst: a });
        for s in [1u8, 2, 4, 8] {
            v.push(X::Lea {
                dst: d,
                base: Some(a),
                index: Some((b, s)),
                disp: 0,
            });
            for &disp in &i32s {
                if disp != 0 {
                    v.push(X::Lea {
                        dst: d,
                        base: Some(a),
                        index: Some((b, s)),
                        disp,
                    });
                }
            }
        }
        for &disp in &i32s {
            if disp != 0 {
                v.push(X::Lea {
                    dst: d,
                    base: Some(a),
                    index: None,
                    disp,
                });
            }
        }
        for &imm in pool {
            v.push(X::MovImm { dst: d, imm });
        }
        if features.iter().any(|f| f == "bmi1") {
            v.push(X::Andn { dst: d, a, b });
        }
        v.into_iter().map(Template::new).collect()
    }
}

impl SynthIsa for Aarch64 {
    fn placeholders() -> [aarch64::Reg; 4] {
        [
            aarch64::Reg(0),
            aarch64::Reg(1),
            aarch64::Reg(2),
            aarch64::Reg(3),
        ]
    }

    fn mov(dst: aarch64::Reg, src: aarch64::Reg) -> aarch64::Inst {
        aarch64::Inst::Mov { dst, src }
    }

    fn templates(pool: &[i64], _features: &[String]) -> Vec<Template<Aarch64>> {
        use aarch64::Inst as A;
        let [n, m, a, d] = Aarch64::placeholders();
        let mut v = Vec::new();
        let amounts: Vec<u8> = {
            let mut s: Vec<u8> = pool
                .iter()
                .filter(|x| (1..64).contains(*x))
                .map(|x| *x as u8)
                .collect();
            s.extend([1, 2, 3, 4, 63]);
            s.sort_unstable();
            s.dedup();
            s
        };
        for op in [
            BinOp::Add,
            BinOp::Sub,
            BinOp::And,
            BinOp::Orr,
            BinOp::Eor,
            BinOp::Bic,
            BinOp::Orn,
            BinOp::Eon,
        ] {
            v.push(A::Bin {
                op,
                dst: d,
                n,
                m,
                shift: None,
            });
            for kind in [ShiftKind::Lsl, ShiftKind::Lsr, ShiftKind::Asr] {
                for &amt in &amounts {
                    v.push(A::Bin {
                        op,
                        dst: d,
                        n,
                        m,
                        shift: Some((kind, amt)),
                    });
                }
            }
        }
        v.push(A::Mul { dst: d, n, m });
        v.push(A::Madd {
            sub: false,
            dst: d,
            n,
            m,
            a,
        });
        v.push(A::Madd {
            sub: true,
            dst: d,
            n,
            m,
            a,
        });
        for kind in [ShiftKind::Lsl, ShiftKind::Lsr, ShiftKind::Asr] {
            for amount in 1..64u8 {
                v.push(A::ShiftImm {
                    kind,
                    dst: d,
                    n,
                    amount,
                });
            }
        }
        for &x in pool {
            if (1..4096).contains(&x) {
                for sub in [false, true] {
                    v.push(A::AddSubImm {
                        sub,
                        dst: d,
                        n,
                        imm: x as u16,
                        lsl12: false,
                    });
                }
            }
            if (0..65536).contains(&x) {
                v.push(A::Movz {
                    dst: d,
                    imm: x as u16,
                    shift: 0,
                });
            }
        }
        v.push(A::Neg {
            dst: d,
            m: n,
            shift: None,
        });
        v.push(A::Mvn { dst: d, m: n });
        v.into_iter().map(Template::new).collect()
    }
}

#[cfg(test)]
mod tests;
