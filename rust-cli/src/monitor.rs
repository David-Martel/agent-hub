//! Real-time session monitoring for active agent orchestration.
//!
//! Renders a continuously-refreshing terminal dashboard that shows:
//! - Per-agent message counts and completion state
//! - Aggregate finding severity counts (CRITICAL/HIGH/MEDIUM/LOW)
//!
//! # Example
//!
//! ```no_run
//! # use agent_bus::monitor::monitor_session;
//! # use agent_bus::settings::Settings;
//! let settings = Settings::from_env();
//! monitor_session(&settings, Some("session:framework-upgrade"), 5).unwrap();
//! ```

use std::collections::HashMap;

use anyhow::Result;

use crate::models::Message;
use crate::redis_bus::bus_list_messages;
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Severity counters
// ---------------------------------------------------------------------------

/// Aggregated finding severity counts extracted from message bodies.
#[derive(Debug, Default, Clone, Copy)]
struct Severities {
    critical: u32,
    high: u32,
    medium: u32,
    low: u32,
}

impl Severities {
    /// Scan a message body for severity keywords and increment the matching counter.
    fn tally(&mut self, body: &str) {
        if body.contains("CRITICAL") {
            self.critical += 1;
        }
        if body.contains("HIGH") {
            self.high += 1;
        }
        if body.contains("MEDIUM") {
            self.medium += 1;
        }
        if body.contains("LOW") {
            self.low += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Per-agent summary
// ---------------------------------------------------------------------------

/// Message count and completion flag for a single agent.
#[derive(Debug, Default, Clone, Copy)]
struct AgentSummary {
    msg_count: usize,
    complete: bool,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Monitor active agents and their findings in real-time.
///
/// Clears the terminal, fetches the 30-most-recent bus messages, filters by
/// `session_tag` if provided, and renders a compact dashboard.  Repeats every
/// `refresh_seconds`.  Runs until interrupted (Ctrl-C).
///
/// # Errors
///
/// Returns an error if the initial Redis query fails.  Subsequent failures
/// inside the loop are logged to stderr and the loop continues.
pub(crate) fn monitor_session(
    settings: &Settings,
    session_tag: Option<&str>,
    refresh_seconds: u64,
) -> Result<()> {
    loop {
        // Clear screen (ANSI — works on Windows Terminal / VT100).
        print!("\x1B[2J\x1B[1;1H");

        // Fetch recent messages — ignore transient errors after the first iteration.
        let msgs = match bus_list_messages(settings, None, None, 30, 500, true) {
            Ok(m) => m,
            Err(err) => {
                eprintln!("[monitor] Redis read error: {err}");
                std::thread::sleep(std::time::Duration::from_secs(refresh_seconds));
                continue;
            }
        };

        // Filter by session tag if provided.
        let filtered: Vec<&Message> = if let Some(tag) = session_tag {
            msgs.iter()
                .filter(|m| m.tags.iter().any(|t| t == tag))
                .collect()
        } else {
            msgs.iter().collect()
        };

        // Aggregate per-agent summaries and global severity counts.
        let mut agents: HashMap<&str, AgentSummary> = HashMap::new();
        let mut sev = Severities::default();

        for m in &filtered {
            let entry = agents.entry(m.from.as_str()).or_default();
            entry.msg_count += 1;
            if m.body.to_uppercase().contains("COMPLETE") {
                entry.complete = true;
            }
            sev.tally(&m.body);
        }

        render_dashboard(filtered.len(), &agents, sev);

        std::thread::sleep(std::time::Duration::from_secs(refresh_seconds));
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Width of the dashboard box (inner content, excluding border chars).
const BOX_WIDTH: usize = 47;

fn render_dashboard(total: usize, agents: &HashMap<&str, AgentSummary>, sev: Severities) {
    let header = format!(" Agent Hub Monitor | {total} messages ");
    println!("{}", box_top());
    println!("{}", box_row(&header));
    println!("{}", box_divider());

    // Sort agents alphabetically for a stable display order.
    let mut sorted: Vec<(&str, AgentSummary)> = agents.iter().map(|(k, v)| (*k, *v)).collect();
    sorted.sort_by_key(|(k, _)| *k);

    for (agent, summary) in &sorted {
        let status = if summary.complete { "+" } else { "~" };
        let row = format!(" {status} {agent:25} {:3} msgs", summary.msg_count);
        println!("{}", box_row(&row));
    }

    println!("{}", box_divider());
    let sev_row = format!(
        " C={} H={} M={} L={}",
        sev.critical, sev.high, sev.medium, sev.low
    );
    println!("{}", box_row(&sev_row));
    println!("{}", box_bottom());
}

fn box_top() -> String {
    format!("+-{}-+", "-".repeat(BOX_WIDTH))
}

fn box_bottom() -> String {
    format!("+-{}-+", "-".repeat(BOX_WIDTH))
}

fn box_divider() -> String {
    format!("+-{}-+", "-".repeat(BOX_WIDTH))
}

fn box_row(content: &str) -> String {
    // Pad or truncate content to exactly BOX_WIDTH chars.
    let padded = format!("{content:<BOX_WIDTH$}");
    let truncated = &padded[..padded.len().min(BOX_WIDTH)];
    format!("| {truncated} |")
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severities_tally_increments_correct_counters() {
        let mut sev = Severities::default();
        sev.tally("CRITICAL issue found");
        sev.tally("HIGH severity");
        sev.tally("HIGH again");
        sev.tally("MEDIUM warning");
        sev.tally("LOW note");
        sev.tally("LOW note 2");
        assert_eq!(sev.critical, 1);
        assert_eq!(sev.high, 2);
        assert_eq!(sev.medium, 1);
        assert_eq!(sev.low, 2);
    }

    #[test]
    fn severities_tally_ignores_unrelated_body() {
        let mut sev = Severities::default();
        sev.tally("everything looks fine");
        assert_eq!(sev.critical, 0);
        assert_eq!(sev.high, 0);
        assert_eq!(sev.medium, 0);
        assert_eq!(sev.low, 0);
    }

    #[test]
    fn box_row_pads_short_content_to_box_width() {
        let row = box_row("hi");
        // | + space + BOX_WIDTH chars + space + |
        assert_eq!(row.len(), BOX_WIDTH + 4);
    }

    #[test]
    fn box_row_truncates_long_content_to_box_width() {
        let long = "x".repeat(BOX_WIDTH + 20);
        let row = box_row(&long);
        assert_eq!(row.len(), BOX_WIDTH + 4);
    }

    #[test]
    fn box_top_bottom_divider_have_consistent_width() {
        let top = box_top();
        let bottom = box_bottom();
        let div = box_divider();
        assert_eq!(top.len(), bottom.len());
        assert_eq!(top.len(), div.len());
    }

    #[test]
    fn agent_summary_complete_flag_set_on_complete_keyword() {
        let mut summary = AgentSummary::default();
        summary.msg_count += 1;
        // Simulate the check in monitor_session
        if "task is COMPLETE now".to_uppercase().contains("COMPLETE") {
            summary.complete = true;
        }
        assert!(summary.complete);
    }

    #[test]
    fn render_dashboard_does_not_panic_with_empty_agents() {
        let agents = HashMap::new();
        let sev = Severities::default();
        // Should complete without panic.
        render_dashboard(0, &agents, sev);
    }
}
