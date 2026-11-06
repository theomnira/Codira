//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The heap policy of every runtime the `codira` tool starts (`run`,
//!   `start`, `test`, `bench` and the launcher of `build --exe`): a finite
//!   heap limit unless `unlimited` is asked for (with a warning), and the
//!   collector's growth percentage.
//! - Where each comes from, in this order: the `--heap-limit`/`--gc-growth`
//!   flags, then the `CODIRA_HEAP_LIMIT`/`CODIRA_GC_GROWTH` environment
//!   variables, then the `[runtime]` table of the package's `codira.toml`
//!   (`heap-limit`, `gc-growth`), then the default: half of the memory
//!   available to the process (see `codira_runtime::heap_policy`) and 100%.
//! - `--runtime-stats`: the collector's statistics, printed when the entry
//!   point returns.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::anyhow;
use codira_project::RuntimeSettings;
use codira_runtime::{
    heap_policy::{self, AvailableMemory},
    GcPolicy, RuntimeBuilder, RuntimeStats,
};

/// The environment variable that sets the heap limit.
pub const HEAP_LIMIT_ENV: &str = "CODIRA_HEAP_LIMIT";
/// The environment variable that sets the collector's growth percentage.
pub const GC_GROWTH_ENV: &str = "CODIRA_GC_GROWTH";

/// The heap flags of the commands that start a runtime.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct HeapArgs {
    /// The most memory the garbage-collected heap may hold: a size (`512MiB`,
    /// `2GiB`, `1500MB`), a percentage of the memory available to the
    /// process (`25%`), or `unlimited`. Takes precedence over
    /// CODIRA_HEAP_LIMIT and over `heap-limit` in the [runtime] table of
    /// codira.toml. Default: 50% of the available memory (the smaller of
    /// physical memory and the cgroup's limit)
    #[clap(long, value_name = "SIZE")]
    pub heap_limit: Option<String>,

    /// Collect once the heap has grown by this percentage over what the
    /// previous collection left live, or `off` to collect only at the heap
    /// limit. Takes precedence over CODIRA_GC_GROWTH and over `gc-growth` in
    /// codira.toml. Default: 100
    #[clap(long, value_name = "PERCENT")]
    pub gc_growth: Option<String>,
}

impl HeapArgs {
    /// The flags to pass on to a child process (`codira test --run-one`,
    /// `codira bench --run-one`), which resolves them again with the same
    /// environment and manifest.
    pub fn forward(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(limit) = &self.heap_limit {
            out.push("--heap-limit".to_string());
            out.push(limit.clone());
        }
        if let Some(growth) = &self.gc_growth {
            out.push("--gc-growth".to_string());
            out.push(growth.clone());
        }
        out
    }
}

/// Where a setting came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A command-line flag.
    Flag,
    /// An environment variable.
    Environment,
    /// The `[runtime]` table of this manifest.
    Manifest(PathBuf),
    /// Nobody chose.
    Default,
}

impl Origin {
    fn describe(&self, flag: &str, env: &str, key: &str) -> String {
        match self {
            Origin::Flag => format!("--{flag}"),
            Origin::Environment => env.to_string(),
            Origin::Manifest(path) => format!("`{key}` in {}", path.display()),
            Origin::Default => "the default".to_string(),
        }
    }
}

/// The heap limit and pacing a runtime gets, and where they came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeapSettings {
    /// The heap limit; `None` only when `unlimited` was asked for.
    pub limit: Option<usize>,
    /// The limit in words, e.g. `6.67 GiB (50% of 13.4 GiB of physical
    /// memory)`.
    pub limit_explanation: String,
    /// Where the limit came from.
    pub limit_origin: Origin,
    /// The collector's growth percentage; `None` for `off`.
    pub growth: Option<u32>,
    /// Where the growth came from.
    pub growth_origin: Origin,
}

/// Picks the first value set, in precedence order.
fn first_set<'a>(
    flag: Option<&'a str>,
    env: Option<&'a str>,
    manifest: Option<(&'a str, &'a Path)>,
) -> Option<(&'a str, Origin)> {
    if let Some(value) = flag {
        return Some((value, Origin::Flag));
    }
    if let Some(value) = env.filter(|v| !v.trim().is_empty()) {
        return Some((value, Origin::Environment));
    }
    manifest.map(|(value, path)| (value, Origin::Manifest(path.to_path_buf())))
}

impl HeapSettings {
    /// Resolves the settings from the flags, the environment and, if there
    /// is one, the package's manifest.
    pub fn resolve(
        args: &HeapArgs,
        manifest: Option<(&RuntimeSettings, &Path)>,
    ) -> anyhow::Result<Self> {
        Self::resolve_with(
            args,
            |name| std::env::var(name).ok(),
            manifest,
            AvailableMemory::detect,
        )
    }

    /// [`HeapSettings::resolve`] with the environment and the memory size
    /// given, for tests.
    pub fn resolve_with(
        args: &HeapArgs,
        env: impl Fn(&str) -> Option<String>,
        manifest: Option<(&RuntimeSettings, &Path)>,
        memory: impl FnOnce() -> Option<AvailableMemory>,
    ) -> anyhow::Result<Self> {
        let env_limit = env(HEAP_LIMIT_ENV);
        let env_growth = env(GC_GROWTH_ENV);
        let manifest_limit =
            manifest.and_then(|(settings, path)| settings.heap_limit.as_deref().map(|v| (v, path)));
        let manifest_growth =
            manifest.and_then(|(settings, path)| settings.gc_growth.as_deref().map(|v| (v, path)));

        let memory = memory();
        let (limit, limit_explanation, limit_origin) = match first_set(
            args.heap_limit.as_deref(),
            env_limit.as_deref(),
            manifest_limit,
        ) {
            Some((text, origin)) => {
                let setting = heap_policy::parse_heap_limit(text).map_err(|err| {
                    anyhow!(
                        "invalid heap limit from {}: {err}",
                        origin.describe("heap-limit", HEAP_LIMIT_ENV, "heap-limit")
                    )
                })?;
                let resolved = heap_policy::resolve_heap_limit(setting, memory.as_ref());
                (resolved.bytes, resolved.explanation, origin)
            }
            None => {
                let resolved = heap_policy::default_heap_limit(memory.as_ref());
                (resolved.bytes, resolved.explanation, Origin::Default)
            }
        };

        let (growth, growth_origin) = match first_set(
            args.gc_growth.as_deref(),
            env_growth.as_deref(),
            manifest_growth,
        ) {
            Some((text, origin)) => {
                let growth = heap_policy::parse_gc_growth(text).map_err(|err| {
                    anyhow!(
                        "invalid collector growth from {}: {err}",
                        origin.describe("gc-growth", GC_GROWTH_ENV, "gc-growth")
                    )
                })?;
                (growth, origin)
            }
            None => (Some(GcPolicy::DEFAULT_GROWTH_PERCENT), Origin::Default),
        };

        Ok(Self {
            limit,
            limit_explanation,
            limit_origin,
            growth,
            growth_origin,
        })
    }

    /// Where the limit came from, in words.
    pub fn limit_source(&self) -> String {
        self.limit_origin
            .describe("heap-limit", HEAP_LIMIT_ENV, "heap-limit")
    }

    /// The lines the runtime prints with the diagnostic of an allocation
    /// that fails at the limit.
    pub fn note(&self) -> String {
        format!(
            "the limit is {}, from {}\n\
             raise it with --heap-limit <SIZE>, the {HEAP_LIMIT_ENV} environment variable, or \
             `heap-limit` in the [runtime] table of codira.toml",
            self.limit_explanation,
            self.limit_source()
        )
    }

    /// Applies the settings to a runtime under construction.
    pub fn apply(&self, mut builder: RuntimeBuilder) -> RuntimeBuilder {
        if let Some(limit) = self.limit {
            builder = builder.heap_limit(limit).heap_limit_note(self.note());
        }
        builder.gc_growth(self.growth)
    }

    /// The warning printed when the heap has no limit, which only happens
    /// when someone asked for it.
    pub fn unlimited_warning(&self) -> Option<String> {
        self.limit.is_none().then(|| {
            format!(
                "warning: the garbage-collected heap has no limit ({}): a program whose live data \
                 keeps growing will grow until the system runs out of memory",
                self.limit_source()
            )
        })
    }

    /// Prints [`HeapSettings::unlimited_warning`] to standard error, if it
    /// applies.
    pub fn warn_if_unlimited(&self) {
        if let Some(warning) = self.unlimited_warning() {
            eprintln!("{warning}");
        }
    }
}

/// The report of `--runtime-stats`: the collector's statistics, the
/// settings and where they came from, and the run's wall time.
pub fn stats_report(stats: &RuntimeStats, settings: &HeapSettings, wall: Duration) -> String {
    let mut out = format!(
        "codira runtime stats (wall time {}):\n",
        codira_runtime::format_duration(wall)
    );
    out.push_str(&format!(
        "  heap limit   {}, from {}\n",
        settings.limit_explanation,
        settings.limit_source()
    ));
    out.push_str(&format!(
        "  gc growth    {}, from {}\n",
        settings
            .growth
            .map_or_else(|| "off".to_string(), |g| format!("{g}%")),
        settings
            .growth_origin
            .describe("gc-growth", GC_GROWTH_ENV, "gc-growth")
    ));
    for line in stats.to_string().lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use codira_runtime::heap_policy::MemorySource;

    use super::*;

    fn memory() -> Option<AvailableMemory> {
        Some(AvailableMemory {
            bytes: 8 << 30,
            source: MemorySource::Physical,
        })
    }

    fn resolve(
        flag: Option<&str>,
        env: &[(&str, &str)],
        manifest: Option<&str>,
    ) -> anyhow::Result<HeapSettings> {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let settings = RuntimeSettings {
            heap_limit: manifest.map(str::to_string),
            gc_growth: None,
        };
        let args = HeapArgs {
            heap_limit: flag.map(str::to_string),
            gc_growth: None,
        };
        HeapSettings::resolve_with(
            &args,
            |name| env.get(name).cloned(),
            Some((&settings, Path::new("/p/codira.toml"))),
            memory,
        )
    }

    #[test]
    fn precedence_is_flag_then_environment_then_manifest_then_default() {
        let all = resolve(Some("1GiB"), &[(HEAP_LIMIT_ENV, "2GiB")], Some("3GiB")).unwrap();
        assert_eq!((all.limit, all.limit_origin), (Some(1 << 30), Origin::Flag));

        let env = resolve(None, &[(HEAP_LIMIT_ENV, "2GiB")], Some("3GiB")).unwrap();
        assert_eq!(
            (env.limit, env.limit_origin),
            (Some(2 << 30), Origin::Environment)
        );

        let toml = resolve(None, &[(HEAP_LIMIT_ENV, " ")], Some("3GiB")).unwrap();
        assert_eq!(toml.limit, Some(3 << 30));
        assert_eq!(toml.limit_origin, Origin::Manifest("/p/codira.toml".into()));

        let default = resolve(None, &[], None).unwrap();
        assert_eq!(default.limit, Some(4 << 30));
        assert_eq!(default.limit_origin, Origin::Default);
        assert_eq!(default.growth, Some(100));
        assert!(default.unlimited_warning().is_none());
    }

    #[test]
    fn unlimited_is_explicit_and_warned_about() {
        let unlimited = resolve(Some("unlimited"), &[], None).unwrap();
        assert_eq!(unlimited.limit, None);
        let warning = unlimited.unlimited_warning().unwrap();
        assert!(warning.contains("no limit (--heap-limit)"), "{warning}");
    }

    #[test]
    fn a_bad_value_names_where_it_came_from() {
        let err = resolve(None, &[(HEAP_LIMIT_ENV, "lots")], None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("from CODIRA_HEAP_LIMIT"), "{err}");
        let err = resolve(None, &[], Some("12 parsecs"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`heap-limit` in /p/codira.toml"), "{err}");
    }

    #[test]
    fn a_percentage_is_of_the_available_memory() {
        let quarter = resolve(Some("25%"), &[], None).unwrap();
        assert_eq!(quarter.limit, Some(2 << 30));
        assert!(quarter.limit_explanation.contains("25% of 8.00 GiB"));
    }

    #[test]
    fn growth_has_its_own_precedence() {
        let env: HashMap<&str, &str> = [(GC_GROWTH_ENV, "off")].into();
        let settings = RuntimeSettings {
            heap_limit: None,
            gc_growth: Some("50".into()),
        };
        let from_env = HeapSettings::resolve_with(
            &HeapArgs::default(),
            |name| env.get(name).map(|v| v.to_string()),
            Some((&settings, Path::new("codira.toml"))),
            memory,
        )
        .unwrap();
        assert_eq!(from_env.growth, None);
        let from_toml = HeapSettings::resolve_with(
            &HeapArgs::default(),
            |_| None,
            Some((&settings, Path::new("codira.toml"))),
            memory,
        )
        .unwrap();
        assert_eq!(from_toml.growth, Some(50));
        let from_flag = HeapSettings::resolve_with(
            &HeapArgs {
                heap_limit: None,
                gc_growth: Some("200".into()),
            },
            |name| env.get(name).map(|v| v.to_string()),
            Some((&settings, Path::new("codira.toml"))),
            memory,
        )
        .unwrap();
        assert_eq!(from_flag.growth, Some(200));
    }
}
