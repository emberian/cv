//! `cv scry` — live activity across harnesses (`tail -f` for sessions).

use crate::util::{home_rel, parse_harness, short_id, Wait};
use anyhow::Result;
use cv_core::ir::{truncate, SessionRef};
use cv_core::watch::{EventKind, Filter, SessionEvent, Watcher};
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_scry(
    harness: Option<String>,
    cwd: Option<String>,
    interval: f64,
    existing: bool,
    follow: bool,
    for_: Option<String>,
) -> Result<()> {
    let interval = poll_interval(interval)?;
    let filter = Filter {
        harness: parse_harness(&harness)?,
        cwd_contains: cwd,
    };
    let wait = Wait::resolve(follow, for_.as_deref())?;
    wait.announce("scry", "live agent activity");
    if matches!(wait, Wait::Once) && !existing {
        // The current state: the sessions a follower would baseline, in the order it polls them.
        for r in Watcher::new(filter, true).discover() {
            println!("{}", header("   ·", &r, r.message_count));
        }
        return Ok(());
    }
    // The baseline read of a large fleet can outlive a short `--for`: it stops at the deadline.
    let mut watcher = Watcher::with_discovery_until(filter, existing, cv_core::discover_all, || wait.expired());
    if !existing && wait.expired() {
        eprintln!(
            "cv scry: --for elapsed during the baseline read, {} sessions in",
            watcher.following()
        );
        return Ok(());
    }
    if !matches!(wait, Wait::Once) {
        eprintln!("cv scry: following {} sessions", watcher.following());
    }
    loop {
        for ev in watcher.poll() {
            print_event(&ev);
        }
        if !wait.pause(interval) {
            return Ok(());
        }
    }
}

fn header(tag: &str, r: &SessionRef, messages: usize) -> String {
    let where_ = r.cwd.as_deref().map(home_rel).unwrap_or_else(|| "?".into());
    format!(
        "{tag}  {:8} {:8}  {}  ({messages} msg)",
        r.harness.as_str(),
        short_id(&r.id),
        where_
    )
}

fn print_event(ev: &SessionEvent) {
    let tag = match ev.kind {
        EventKind::New => "✷ new",
        EventKind::Updated => "   +",
    };
    println!("{}", header(tag, &ev.reference, ev.new_messages.len()));
    for m in &ev.new_messages {
        if let Some(t) = m.text() {
            println!("      {} {}", cv_core::render::role_label(m.role), truncate(&t, 100));
        }
    }
}

fn poll_interval(seconds: f64) -> Result<Duration> {
    if !seconds.is_finite() {
        return crate::util::usage("--interval must be finite");
    }
    Duration::try_from_secs_f64(seconds.max(0.25))
        .map_err(|_| crate::util::UsageError("--interval exceeds the supported duration".into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scry_interval_rejects_nonfinite_and_overflow_without_panicking() {
        for seconds in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
            assert!(poll_interval(seconds)
                .unwrap_err()
                .downcast_ref::<crate::util::UsageError>()
                .is_some());
        }
        assert_eq!(poll_interval(0.0).unwrap(), Duration::from_millis(250));
        assert_eq!(poll_interval(-1.0).unwrap(), Duration::from_millis(250));
        assert_eq!(poll_interval(1.5).unwrap(), Duration::from_millis(1500));
    }
}
