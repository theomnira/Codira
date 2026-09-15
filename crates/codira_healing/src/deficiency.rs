//! Healing Deficiency Reports (HDR): structured diagnostics emitted when the
//! Bayesian ranker finds all strategies for a site failing too often.
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality (per Section 6.1 of the HERACLES spec):
//! - `HealingDeficiencyReport`: a structured report describing a site whose
//!   healing is under-performing, mirroring the JSON shape in the spec
//!   (`site_id`, module, `fault_class`, `dominant_strategy_success_rate`,
//!   `window_size`, `suggested_action`).
//! - `DeficiencyDetector`: watches fingerprint observations over a sliding
//!   window and emits an HDR when the dominant strategy's success rate falls
//!   below a configurable threshold `θ` (default 0.10) over a window of `W`
//!   fault events (default 100).
//! - HDRs close the loop between runtime observation and compile-time code
//!   generation (Section 6.1: "can be fed to the AOT compiler to trigger an
//!   incremental recompilation").

/// A structured Healing Deficiency Report, per the JSON shape in Section 6.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealingDeficiencyReport {
    /// The site whose healing is deficient (hex string, matching the spec's
    /// `0xDEADBEEF` example).
    pub site_id: String,
    /// The module the site belongs to, e.g. `network::http::client`.
    pub module: String,
    /// The dominant fault class at the site, e.g. `ConnectionRefused`.
    pub fault_class: String,
    /// The success rate of the currently-dominant strategy.
    pub dominant_strategy_success_rate: u64,
    /// The window over which the rate was measured (fault events).
    pub window_size: u64,
    /// A suggested action for the AOT compiler / engineer.
    pub suggested_action: String,
}

impl HealingDeficiencyReport {
    /// Serializes the report to a JSON-ish string for logging. This is a
    /// deliberately simple hand-rolled serializer (the crate doesn't pull
    /// in serde); the field names match the spec's example exactly.
    pub fn to_json(&self) -> String {
        format!(
            "{{\n  \"site_id\": \"{}\",\n  \"module\": \"{}\",\n  \"fault_class\": \"{}\",\n  \
             \"dominant_strategy_success_rate\": {},\n  \"window_size\": {},\n  \
             \"suggested_action\": \"{}\"\n}}",
            self.site_id,
            self.module,
            self.fault_class,
            self.dominant_strategy_success_rate,
            self.window_size,
            self.suggested_action
        )
    }
}

/// Detects healing deficiencies from sliding-window success-rate tracking.
///
/// The detector keeps a sliding window of recent recovery outcomes per
/// site and emits an HDR the first time the dominant strategy's success
/// rate over that window drops below the threshold. It re-arms once the
/// window no longer contains enough events to make a judgment.
pub struct DeficiencyDetector {
    /// The success-rate threshold θ below which healing is "deficient".
    threshold: f64,
    /// The window size W in fault events.
    window_size: usize,
    /// Recent outcomes per site: (strategy name, succeeded).
    recent: Vec<(u64, String, bool)>,
    /// Sites that already emitted an HDR since their last re-arm.
    reported: Vec<u64>,
}

impl DeficiencyDetector {
    pub fn new() -> Self {
        DeficiencyDetector {
            threshold: 0.10,
            window_size: 100,
            recent: Vec::new(),
            reported: Vec::new(),
        }
    }

    /// Configures the detection threshold and window.
    pub fn with_params(mut self, threshold: f64, window_size: usize) -> Self {
        self.threshold = threshold;
        self.window_size = window_size;
        self
    }

    /// Observes one recovery outcome at `site_id` for `strategy`. Returns
    /// a deficiency report if, after this observation, the dominant
    /// strategy's success rate over the last `window_size` events has
    /// dropped below the threshold and the site hasn't reported recently.
    pub fn observe(
        &mut self,
        site_id: u64,
        strategy: &str,
        succeeded: bool,
    ) -> Option<HealingDeficiencyReport> {
        self.recent.push((site_id, strategy.to_owned(), succeeded));
        if self.recent.len() > self.window_size {
            self.recent.remove(0);
        }

        if self.recent.len() < self.window_size {
            return None;
        }

        // Only look at this site's events within the window.
        let site_events = self
            .recent
            .iter()
            .filter(|(sid, _, _)| *sid == site_id)
            .collect::<Vec<_>>();

        if site_events.len() < self.window_size {
            return None;
        }

        // Find the dominant strategy (most attempted) and its success rate.
        let mut attempts: std::collections::HashMap<&str, (u32, u32)> =
            std::collections::HashMap::new();
        for (_, strategy, succeeded) in &site_events {
            let entry = attempts.entry(strategy.as_str()).or_insert((0, 0));
            entry.0 += 1;
            if *succeeded {
                entry.1 += 1;
            }
        }
        let (_dominant, (attempts, successes)) = attempts
            .iter()
            .max_by(|a, b| a.1 .0.cmp(&b.1 .0))
            .map(|(k, v)| (*k, *v))?;

        let rate = f64::from(successes) / f64::from(attempts);
        if rate < self.threshold && !self.reported.contains(&site_id) {
            self.reported.push(site_id);
            return Some(HealingDeficiencyReport {
                site_id: format!("0x{site_id:08X}"),
                module: "unknown".to_owned(),
                fault_class: "unspecified".to_owned(),
                dominant_strategy_success_rate: (rate * 100.0) as u64,
                window_size: self.window_size as u64,
                suggested_action: "add_strategy:SubstituteAlternate or extend_contract".to_owned(),
            });
        }

        None
    }

    /// Re-arms the detector for a site so it can emit another report.
    /// Drops the site's buffered events too, so a fresh full window of
    /// observations is required before a new report can fire.
    pub fn rearm(&mut self, site_id: u64) {
        self.reported.retain(|s| *s != site_id);
        self.recent.retain(|(sid, _, _)| *sid != site_id);
    }
}

impl Default for DeficiencyDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_report_until_window_is_full() {
        let mut detector = DeficiencyDetector::new().with_params(0.10, 10);
        for _ in 0..9 {
            assert!(detector.observe(1, "retry", false).is_none());
        }
        // 10 events, all failures: now the window is full and the rate is 0.
        let report = detector.observe(1, "retry", false);
        assert!(report.is_some(), "window full + 0% rate should report");
    }

    #[test]
    fn healthy_site_does_not_report() {
        let mut detector = DeficiencyDetector::new().with_params(0.10, 10);
        let mut report = None;
        for _ in 0..10 {
            report = detector.observe(1, "retry", true);
        }
        assert!(
            report.is_none(),
            "healthy site must not report, got {report:?}"
        );
    }

    #[test]
    fn reports_once_then_requires_rearm() {
        let mut detector = DeficiencyDetector::new().with_params(0.10, 10);
        let mut first = None;
        for _ in 0..10 {
            first = detector.observe(1, "retry", false);
        }
        assert!(first.is_some());

        // More failing events must not spam a second report until rearm.
        let mut second = None;
        for _ in 0..10 {
            second = detector.observe(1, "retry", false);
        }
        assert!(second.is_none(), "should not re-report without rearm");

        detector.rearm(1);
        let mut third = None;
        for _ in 0..10 {
            third = detector.observe(1, "retry", false);
        }
        assert!(third.is_some(), "rearmed site should report again");
    }

    #[test]
    fn json_shape_matches_spec_example() {
        let report = HealingDeficiencyReport {
            site_id: "0xDEADBEEF".to_owned(),
            module: "network::http::client".to_owned(),
            fault_class: "ConnectionRefused".to_owned(),
            dominant_strategy_success_rate: 4,
            window_size: 100,
            suggested_action: "add_strategy:SubstituteAlternate or extend_contract".to_owned(),
        };
        let json = report.to_json();
        assert!(json.contains("\"site_id\": \"0xDEADBEEF\""));
        assert!(json.contains("\"module\": \"network::http::client\""));
        assert!(json.contains("\"fault_class\": \"ConnectionRefused\""));
        assert!(json.contains("\"dominant_strategy_success_rate\": 4"));
        assert!(json.contains("\"window_size\": 100"));
    }
}
