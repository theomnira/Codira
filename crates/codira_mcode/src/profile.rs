//! Versioned microarchitecture cost profiles and the block cost estimate.
//!
//! A profile maps every instruction *form* (see [`crate::Isa::form`]) to its
//! latency, reciprocal throughput, micro-op count and the execution ports
//! the micro-ops may issue to. Every profile states where its numbers come
//! from: measured on a named machine by [`crate::measure`], or transcribed
//! from a named document. A profile is plain text (see [`Profile::parse`])
//! so it can be checked into the repository, reviewed and diffed.
//!
//! The estimate ([`Profile::cost`]) is the usual static model, in the
//! spirit of `llvm-mca` and of uops.info's throughput predictor, but much
//! simpler: the critical-path latency from live-ins to live-outs, and a
//! steady-state throughput bound that is the maximum of the issue-width
//! bound and, for every union `U` of the port sets in use, the micro-ops
//! that can only run on `U` divided by `|U|`. It ignores the front end
//! beyond issue width, memory, and scheduler effects, which is why a choice
//! made with it is measured before it is believed ([`crate::measure`]).

use std::collections::BTreeMap;
use std::fmt;

use crate::{Block, Isa};

#[derive(Clone, Debug, PartialEq)]
pub struct FormCost {
    /// Cycles from the last input being ready to the result being ready.
    pub latency: f64,
    /// Cycles per instruction when many independent ones are issued.
    pub rthroughput: f64,
    /// Micro-ops issued.
    pub uops: u32,
    /// Ports any of the micro-ops may use (empty: none, e.g. an eliminated
    /// move).
    pub ports: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// Versioned identity, e.g. `x86_64/emerald-rapids-vm@1`. Part of every
    /// cache key and every recorded decision.
    pub id: String,
    pub isa: String,
    /// Micro-ops renamed/issued per cycle.
    pub issue_width: u32,
    /// Extensions the deployment target has. Instructions needing others
    /// are never emitted for this profile.
    pub features: Vec<String>,
    /// Where the numbers come from (free text, required).
    pub source: String,
    pub forms: BTreeMap<String, FormCost>,
}

/// The estimated cost of a block under a profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cost {
    /// Critical path, cycles.
    pub latency: f64,
    /// Steady-state cycles per execution of the block.
    pub throughput: f64,
    pub uops: u32,
    pub bytes: u32,
    pub insts: u32,
}

/// What the superoptimiser minimises. Ties are broken by the other metric,
/// then code size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Objective {
    Latency,
    Throughput,
}

impl Cost {
    /// A key that orders costs under `objective` (smaller is better).
    pub fn key(&self, objective: Objective) -> (u64, u64, u32, u32) {
        let q = |x: f64| (x * 1000.0).round() as u64;
        match objective {
            Objective::Latency => (q(self.latency), q(self.throughput), self.uops, self.bytes),
            Objective::Throughput => (q(self.throughput), q(self.latency), self.uops, self.bytes),
        }
    }
}

impl fmt::Display for Cost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "latency {:.2} cy, throughput {:.2} cy/block, {} uops, {} insts, {} bytes",
            self.latency, self.throughput, self.uops, self.insts, self.bytes
        )
    }
}

impl Profile {
    /// Parses the text format written by [`fmt::Display`]:
    ///
    /// ```text
    /// profile x86_64/example@1
    /// isa x86_64
    /// issue_width 6
    /// features bmi1 bmi2
    /// source measured on ...
    /// form add r64, r64 | 1.0 | 0.25 | 1 | 0 1 5 6
    /// ```
    pub fn parse(text: &str) -> Result<Profile, String> {
        let mut p = Profile {
            id: String::new(),
            isa: String::new(),
            issue_width: 0,
            features: Vec::new(),
            source: String::new(),
            forms: BTreeMap::new(),
        };
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
            let rest = rest.trim();
            let err = |m: &str| format!("profile line {}: {m}: `{line}`", n + 1);
            match key {
                "profile" => p.id = rest.to_string(),
                "isa" => p.isa = rest.to_string(),
                "issue_width" => {
                    p.issue_width = rest.parse().map_err(|_e| err("bad issue width"))?;
                }
                "features" => p.features = rest.split_whitespace().map(String::from).collect(),
                "source" => {
                    if !p.source.is_empty() {
                        p.source.push(' ');
                    }
                    p.source.push_str(rest);
                }
                "form" => {
                    let cols: Vec<&str> = rest.split('|').map(str::trim).collect();
                    let [name, lat, tp, uops, ports] = cols.as_slice() else {
                        return Err(err(
                            "expected `form NAME | latency | rthroughput | uops | ports`",
                        ));
                    };
                    let cost = FormCost {
                        latency: lat.parse().map_err(|_e| err("bad latency"))?,
                        rthroughput: tp.parse().map_err(|_e| err("bad throughput"))?,
                        uops: uops.parse().map_err(|_e| err("bad uop count"))?,
                        ports: ports
                            .split_whitespace()
                            .map(|s| s.parse::<u8>().map_err(|_e| err("bad port")))
                            .collect::<Result<_, _>>()?,
                    };
                    p.forms.insert(name.to_string(), cost);
                }
                _ => return Err(err("unknown key")),
            }
        }
        if p.id.is_empty() || p.isa.is_empty() || p.issue_width == 0 || p.source.is_empty() {
            return Err("profile needs `profile`, `isa`, `issue_width` and `source`".into());
        }
        Ok(p)
    }

    /// Whether every instruction of the block has a cost entry and only
    /// uses features the profile's target has.
    pub fn supports<I: Isa>(&self, block: &Block<I>) -> Result<(), String> {
        if self.isa != I::NAME {
            return Err(format!(
                "profile {} is for {}, block is {}",
                self.id,
                self.isa,
                I::NAME
            ));
        }
        for inst in &block.insts {
            if !self.forms.contains_key(I::form(inst)) {
                return Err(format!(
                    "profile {} has no cost for form `{}` (`{inst}`)",
                    self.id,
                    I::form(inst)
                ));
            }
            if let Some(f) = I::required_feature(inst) {
                if !self.features.iter().any(|x| x == f) {
                    return Err(format!(
                        "`{inst}` needs {f}, which target {} does not have",
                        self.id
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn form_cost<I: Isa>(&self, inst: &I::Inst) -> Option<&FormCost> {
        self.forms.get(I::form(inst))
    }

    /// The cost estimate of a block (see the module documentation).
    pub fn cost<I: Isa>(&self, block: &Block<I>) -> Result<Cost, String> {
        self.supports(block)?;
        // Critical path.
        let mut ready: BTreeMap<I::Reg, f64> = block.live_in.iter().map(|r| (*r, 0.0)).collect();
        let mut uops = 0;
        let mut port_uops: Vec<(Vec<u8>, f64)> = Vec::new();
        for inst in &block.insts {
            let c = self.form_cost::<I>(inst).expect("checked by supports");
            let start = I::reads(inst)
                .iter()
                .map(|r| ready.get(r).copied().unwrap_or(0.0))
                .fold(0.0, f64::max);
            ready.insert(I::writes(inst), start + c.latency);
            uops += c.uops;
            if !c.ports.is_empty() {
                // A form whose measured reciprocal throughput is worse than
                // its port count allows is charged accordingly.
                let demand = (c.rthroughput * c.ports.len() as f64).max(f64::from(c.uops));
                port_uops.push((c.ports.clone(), demand));
            }
        }
        let latency = block
            .live_out
            .iter()
            .map(|r| ready.get(r).copied().unwrap_or(0.0))
            .fold(0.0, f64::max);
        // Port-pressure bound over unions of the port sets in use.
        let mut sets: Vec<Vec<u8>> = port_uops.iter().map(|(p, _)| p.clone()).collect();
        sets.sort();
        sets.dedup();
        let mut bound = f64::from(uops) / f64::from(self.issue_width);
        let n = sets.len().min(12);
        for mask in 1u32..(1 << n) {
            let mut union: Vec<u8> = Vec::new();
            for (i, s) in sets.iter().enumerate().take(n) {
                if mask & (1 << i) != 0 {
                    union.extend(s);
                }
            }
            union.sort_unstable();
            union.dedup();
            let demand: f64 = port_uops
                .iter()
                .filter(|(p, _)| p.iter().all(|x| union.contains(x)))
                .map(|(_, d)| d)
                .sum();
            bound = bound.max(demand / union.len() as f64);
        }
        Ok(Cost {
            latency,
            throughput: bound,
            uops,
            bytes: block.encoded_len(),
            insts: block.insts.len() as u32,
        })
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "profile {}", self.id)?;
        writeln!(f, "isa {}", self.isa)?;
        writeln!(f, "issue_width {}", self.issue_width)?;
        writeln!(f, "features {}", self.features.join(" "))?;
        writeln!(f, "source {}", self.source)?;
        for (name, c) in &self.forms {
            let ports: Vec<String> = c.ports.iter().map(u8::to_string).collect();
            writeln!(
                f,
                "form {name} | {} | {} | {} | {}",
                c.latency,
                c.rthroughput,
                c.uops,
                ports.join(" ")
            )?;
        }
        Ok(())
    }
}
