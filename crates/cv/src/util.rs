//! Shared CLI helpers: id resolution (`harness:id`, ambiguity → exit 2), the read-window grammar
//! (`--first/--last/--range A..B/--around`), the session-row JSON shape, and small display utilities.

use anyhow::{bail, Context, Result};
use cv_core::ir::{Harness, Message, SessionRef};
use cv_core::{Adapter, Flow, MessageSink, ParseOptions};
use std::path::Path;

// ---------- exit codes ----------

/// A caller mistake the CLI answers with exit code 2 (clap's own usage-error code): an unknown or
/// ambiguous id, a removed command or flag, a bad window spec. `main` prints the message bare (no
/// `Error:` chain) and exits 2, so scripts can tell "you asked wrong" from "it broke" (exit 1).
#[derive(Debug)]
pub(crate) struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// `Err(UsageError)` as an `anyhow::Result`, for `return usage(...)`.
pub(crate) fn usage<T>(msg: impl Into<String>) -> Result<T> {
    Err(UsageError(msg.into()).into())
}

/// How to publish a file the user named (`--out`, `--pack`): a taken name is refused (exit 2)
/// unless `--force`, which replaces it by atomic rename. Never written in place.
pub(crate) fn user_file_claim(path: &Path, force: bool) -> Result<cv_core::publish::Claim> {
    if force {
        return Ok(cv_core::publish::Claim::Replace);
    }
    cv_core::publish::refuse_existing(path, " (pass --force to replace it)")?;
    Ok(cv_core::publish::Claim::New)
}

/// Publish `bytes` at a file the user named (see [`user_file_claim`]).
pub(crate) fn write_user_file(path: &Path, bytes: &[u8], force: bool) -> Result<()> {
    let claim = user_file_claim(path, force)?;
    cv_core::publish::publish_reader(path, "out", bytes, claim)
}

// ---------- waiting ----------

/// Whether a command that could wait does (docs/INTERFACE-V2.md §2, "blocking is opt-in and
/// announced"): `--follow` or `--for` asks for it, a terminal on stdout permits it, and a pipe
/// without either gets the current state once. The announcement goes out before the first
/// discovery, so it lands within a second on a cold fleet too.
pub(crate) enum Wait {
    /// Print the current state once and exit 0.
    Once,
    /// Keep polling until the `--for` deadline (with the length asked for), or forever.
    Until(Option<(std::time::Instant, std::time::Duration)>),
}

impl Wait {
    pub(crate) fn resolve(follow: bool, for_: Option<&str>) -> Result<Wait> {
        let bound = for_.map(parse_for).transpose()?;
        if follow || bound.is_some() || std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            Ok(Wait::Until(bound.map(|d| (std::time::Instant::now() + d, d))))
        } else {
            Ok(Wait::Once)
        }
    }

    /// The one stderr line: what `cv <cmd>` is waiting for, or how to make it wait.
    pub(crate) fn announce(&self, cmd: &str, what: &str) {
        match self {
            Wait::Once => eprintln!(
                "cv {cmd}: stdout is not a terminal, so this is the current state once; \
                 --follow keeps following {what}, --for <duration> bounds it"
            ),
            Wait::Until(None) => eprintln!("cv {cmd}: following {what}; stop with Ctrl-C or --for <duration>"),
            Wait::Until(Some((_, length))) => eprintln!(
                "cv {cmd}: following {what} for {}; Ctrl-C stops early",
                fmt_duration(length.as_millis() as u64)
            ),
        }
    }

    /// Whether the `--for` deadline has passed (never, without one).
    pub(crate) fn expired(&self) -> bool {
        matches!(self, Wait::Until(Some((deadline, _))) if *deadline <= std::time::Instant::now())
    }

    /// Sleep one `interval`, cut short by the deadline; false once there is nothing left to wait for.
    pub(crate) fn pause(&self, interval: std::time::Duration) -> bool {
        let left = match self {
            Wait::Once => return false,
            Wait::Until(None) => interval,
            Wait::Until(Some((deadline, _))) => {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return false;
                }
                interval.min(left)
            }
        };
        std::thread::sleep(left);
        true
    }
}

/// `--for 30s` / `5m` / `2h` (the task deadline grammar) as a duration; anything else is exit 2.
fn parse_for(s: &str) -> Result<std::time::Duration> {
    match cv_core::task::project::parse_duration(s).and_then(|d| d.to_std().ok()) {
        Some(d) => Ok(d),
        None => usage(format!("--for wants a duration such as 30s, 5m or 2h, not {s:?}")),
    }
}

// ---------- harness / id resolution ----------

pub(crate) fn parse_harness(s: &Option<String>) -> Result<Option<Harness>> {
    match s {
        None => Ok(None),
        Some(s) => match Harness::parse(s) {
            Some(h) => Ok(Some(h)),
            None => match Harness::retired(s) {
                Some(where_now) => usage(format!("{s} is no longer a harness: {where_now}")),
                None => usage(format!("unknown harness: {s}")),
            },
        },
    }
}

/// Split an optional `<harness>:<id>` prefix off an id. Only treats the part before the first `:`
/// as a harness when it actually names one; otherwise the whole string is the id and `fallback`
/// (a `--harness` flag) applies. Every command that takes an id accepts this form.
pub(crate) fn split_harness_id(spec: &str, fallback: Option<Harness>) -> (Option<Harness>, &str) {
    if let Some((head, rest)) = spec.split_once(':') {
        if let Some(h) = Harness::parse(head) {
            return (Some(h), rest);
        }
    }
    (fallback, spec)
}

/// The one way a command turns an id into a session: `harness:id` or a unique id prefix, optionally
/// constrained by `--harness`. An unknown id is a [`UsageError`] (exit 2); an ambiguous prefix lists
/// every candidate as a `harness:full-id` line and exits 2 too, so the caller can paste one back.
pub(crate) fn resolve(spec: &str, harness: Option<Harness>) -> Result<(SessionRef, Box<dyn Adapter>)> {
    let (want, id) = split_harness_id(spec, harness);
    resolve_found(cv_core::find(id, want), id, want)
}

/// Map a `find`/`find_cheap` outcome to the CLI contract: `None` → unknown (exit 2), the core's
/// ambiguity error → candidate lines (exit 2), anything else → a real error (exit 1).
pub(crate) fn resolve_found(
    found: Result<Option<(SessionRef, Box<dyn Adapter>)>>,
    id: &str,
    want: Option<Harness>,
) -> Result<(SessionRef, Box<dyn Adapter>)> {
    match found {
        Ok(Some(hit)) => Ok(hit),
        Ok(None) => usage(format!("no session matching {id:?}")),
        Err(e) if is_ambiguity(&e) => usage(ambiguous_message(id, want, &e)),
        Err(e) => Err(e),
    }
}

fn is_ambiguity(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.starts_with("ambiguous session id") || s.contains("sessions have a sub-agent matching")
}

/// `harness:full-id` candidate lines for an ambiguous prefix, from the probed catalog. Falls back
/// to the core's own message when the candidates aren't catalog sessions (e.g. sub-agent ids).
fn ambiguous_message(id: &str, want: Option<Harness>, core_err: &anyhow::Error) -> String {
    let mut rows: Vec<String> = cv_core::sessions()
        .into_iter()
        .filter(|r| want.is_none_or(|h| r.harness == h))
        .filter(|r| r.id.starts_with(id))
        .map(|r| format!("{}:{}", r.harness.as_str(), r.id))
        .collect();
    rows.sort();
    rows.dedup();
    if rows.len() < 2 {
        return core_err.to_string();
    }
    format!(
        "ambiguous id {id:?} — {} candidates (pass a longer prefix or harness:id):\n{}",
        rows.len(),
        rows.join("\n")
    )
}

// ---------- read windows ----------

/// The read-window flags shared by `show`, `export` and everything else that renders a message
/// range. Implemented once here and `#[command(flatten)]`ed in; the grammar is the same everywhere:
/// 0-based, end-exclusive, exactly one selector at a time.
#[derive(clap::Args, Debug, Default, Clone)]
pub(crate) struct WindowArgs {
    /// The first N messages.
    #[arg(long, value_name = "N", conflicts_with_all = ["last", "range", "around"])]
    pub first: Option<usize>,
    /// The last N messages.
    #[arg(long, value_name = "N", conflicts_with_all = ["range", "around"])]
    pub last: Option<usize>,
    /// Messages A (inclusive) to B (exclusive), 0-based: `A..B`; `A..` to the end, `..B` from the start.
    #[arg(long, value_name = "A..B", allow_hyphen_values = true, conflicts_with = "around")]
    pub range: Option<String>,
    /// Message N with --context K messages either side.
    #[arg(long, value_name = "N")]
    pub around: Option<usize>,
    /// How many messages either side of --around N (default 5).
    #[arg(long, value_name = "K", default_value_t = 5, requires = "around")]
    pub context: usize,
    /// Stop after N bytes of rendered output and print `… continue with --range <next>..`.
    #[arg(long, value_name = "N")]
    pub max_bytes: Option<usize>,
}

/// One resolved selector. `Last` needs the message count, which the caller supplies lazily (a
/// streaming count under lazy parse options — no content is read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Window {
    First(usize),
    Last(usize),
    Range(usize, Option<usize>),
    Around { center: usize, context: usize },
}

impl WindowArgs {
    /// The selector these flags spell, if any. clap already rejects two selectors at once; this
    /// only parses `--range`.
    pub(crate) fn window(&self) -> Result<Option<Window>> {
        if let Some(n) = self.first {
            return Ok(Some(Window::First(n)));
        }
        if let Some(n) = self.last {
            return Ok(Some(Window::Last(n)));
        }
        if let Some(spec) = &self.range {
            let (a, b) = parse_range(spec)?;
            return Ok(Some(Window::Range(a, b)));
        }
        if let Some(center) = self.around {
            return Ok(Some(Window::Around {
                center,
                context: self.context,
            }));
        }
        Ok(None)
    }
}

impl Window {
    /// Resolve to `[start, end)`; `end == None` means "through the last message".
    pub(crate) fn bounds(self, total: impl FnOnce() -> Result<usize>) -> Result<(usize, Option<usize>)> {
        Ok(match self {
            Window::First(n) => (0, Some(n)),
            Window::Last(n) => {
                let total = total()?;
                (total.saturating_sub(n), None)
            }
            Window::Range(a, b) => (a, b),
            Window::Around { center, context } => (
                center.saturating_sub(context),
                Some(center.saturating_add(context).saturating_add(1)),
            ),
        })
    }
}

/// Parse the `A..B` window grammar: `A..B` (0-based, end-exclusive), `A..` (through the last),
/// `..B` (from the first). The 0.10 `A-B` / `-N` / `N-` / bare `N` forms are rejected with a
/// pointer to what replaced them.
pub(crate) fn parse_range(spec: &str) -> Result<(usize, Option<usize>)> {
    parse_range_inner(spec).map_err(|error| UsageError(error.to_string()).into())
}

fn parse_range_inner(spec: &str) -> Result<(usize, Option<usize>)> {
    let spec = spec.trim();
    let Some((a, b)) = spec.split_once("..") else {
        if spec.parse::<usize>().is_ok() {
            bail!(
                "bad range {spec:?}: a single message is `--around {spec} --context 0` (or `{spec}..{}`)",
                next_index(spec)
            );
        }
        if spec.starts_with('-') || spec.ends_with('-') || spec.contains('-') {
            bail!(
                "bad range {spec:?}: the `A-B`/`-N`/`N-` grammar is gone — use `--range A..B` (0-based, \
                 end-exclusive), `A..`, `..B`; `--first N` for the first N, `--last N` for the last N"
            );
        }
        bail!("bad range {spec:?}: expected A..B, A.., or ..B (0-based, end-exclusive)");
    };
    let start = if a.trim().is_empty() {
        0
    } else {
        a.trim()
            .parse()
            .with_context(|| format!("bad range {spec:?}: start must be a number"))?
    };
    let end = if b.trim().is_empty() {
        None
    } else {
        Some(
            b.trim()
                .parse()
                .with_context(|| format!("bad range {spec:?}: end must be a number"))?,
        )
    };
    if let Some(end) = end {
        if end < start {
            bail!("bad range {spec:?}: end ({end}) is before start ({start})");
        }
    }
    Ok((start, end))
}

fn next_index(n: &str) -> String {
    n.parse::<usize>()
        .ok()
        .and_then(|n| n.checked_add(1))
        .map(|n| n.to_string())
        .unwrap_or_default()
}

/// Clamp a `[start, end)` window to `len` messages (a past-the-end window is empty, never a panic).
#[cfg(test)]
pub(crate) fn clamp(range: (usize, Option<usize>), len: usize) -> (usize, usize) {
    let end = range.1.unwrap_or(len).min(len);
    (range.0.min(end), end)
}

/// The IR message count of a session, by one streamed pass under lazy options (content stays on
/// disk). `SessionRef::message_count` counts conversational turns only, so `--last N` can't use it.
pub(crate) fn count_messages(adapter: &dyn Adapter, r: &SessionRef) -> Result<usize> {
    struct Count(usize);
    impl MessageSink for Count {
        fn message(&mut self, _m: Message) -> Flow {
            self.0 += 1;
            Flow::Continue
        }
    }
    let mut c = Count(0);
    adapter.try_stream(r, &ParseOptions::lazy(), &mut c)?;
    Ok(c.0)
}

/// The continuation line printed when `--max-bytes` cut a render short.
pub(crate) fn continue_hint(next: usize) -> String {
    format!("… continue with --range {next}..")
}

// ---------- the session row ----------

/// The one JSON shape for a session in a list (`ls`, `search`): snake_case, always
/// these keys. It lives in cv-core because the CLI is not the only door onto it — `cv serve`'s
/// `/api/search` emits the same row, and §3 only holds if there is one implementation.
pub(crate) use cv_core::rows::session_row;

// ---------- display ----------

pub(crate) fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Format a UTC instant in the **local** timezone. Every human-facing time cv prints goes through
/// here (or [`fmt_local_ts`]): transcripts store UTC, but the user's other clocks — `git log`,
/// their memory of the evening — are local, and silently mixing the two skews forensics by the
/// UTC offset (a real 4-hour miss reconstructing a power-loss window prompted this).
pub(crate) fn fmt_local(d: chrono::DateTime<chrono::Utc>, fmt: &str) -> String {
    d.with_timezone(&chrono::Local).format(fmt).to_string()
}

/// [`fmt_local`] from unix seconds (the events catalog's timestamp shape).
pub(crate) fn fmt_local_ts(secs: i64, fmt: &str) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(|d| fmt_local(d, fmt))
}

/// A session's `created → last-active` span, local time, fixed width (pads to [`SPAN_WIDTH`]).
/// Same-day sessions compress the right side to its time; a missing `created` shows only the
/// last-active instant. The span is what distinguishes a long-lived orchestrator from a one-shot —
/// and, after a crash, the resumed from the dropped.
pub(crate) const SPAN_WIDTH: usize = 31;
pub(crate) fn fmt_span(
    created: Option<chrono::DateTime<chrono::Utc>>,
    updated: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    const FMT: &str = "%y-%m-%d %H:%M";
    let s = match (created, updated) {
        (Some(c), Some(u)) => {
            let (lc, lu) = (c.with_timezone(&chrono::Local), u.with_timezone(&chrono::Local));
            if lc.date_naive() == lu.date_naive() {
                format!("{} → {}", lc.format(FMT), lu.format("%H:%M"))
            } else {
                format!("{} → {}", lc.format(FMT), lu.format(FMT))
            }
        }
        (Some(t), None) | (None, Some(t)) => fmt_local(t, FMT),
        (None, None) => "?".into(),
    };
    format!("{s:<SPAN_WIDTH$}")
}

pub(crate) use cv_core::ir::home_rel;

pub(crate) fn dim_cwd(p: Option<&Path>) -> String {
    p.map(home_rel).unwrap_or_else(|| "(no cwd)".into())
}

pub(crate) fn dirs_home() -> Option<std::path::PathBuf> {
    cv_core::env::var_os("HOME").map(std::path::PathBuf::from)
}

// ---------- compose-family reports ----------

/// The compose-family report (`prune`, `rewind`, `distill`, `splice --session`, `adopt`): the human account on
/// stderr and, under `--json`, ONE JSON object on stdout — so stdout carries only the JSON and pipes
/// cleanly.
pub(crate) fn report(json: bool, human: &str, value: &serde_json::Value) -> Result<()> {
    eprint!("{human}");
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

/// Dry-run honesty for a `--json` report: nothing was written, so every `paths` key is null, and
/// the `minted` keys (the new id and what derives from it) are null too unless `--to` pinned the
/// id — the core mints a throwaway id a real run would NOT reuse. `note` says which.
pub(crate) fn dry_run_nulls(v: &mut serde_json::Value, pinned: bool, paths: &[&str], minted: &[&str]) {
    v["note"] = serde_json::Value::Null;
    if v["dry_run"] != true {
        return;
    }
    for k in paths {
        v[*k] = serde_json::Value::Null;
    }
    if !pinned {
        for k in minted {
            v[*k] = serde_json::Value::Null;
        }
    }
    v["note"] = if pinned {
        "dry run — nothing written"
    } else {
        "dry run — nothing written; a real run mints a fresh session id (pass --to to pin one)"
    }
    .into();
}

/// `--thinking <native|text|drop>`: what an emit does with the model's reasoning on the way out.
/// Shared by every command that writes a session into a harness (`port`, `splice`,
/// `pack --format session`), because a flag means the same thing everywhere it appears.
pub(crate) fn parse_thinking(s: &str) -> Result<cv_core::emit::ThinkingMode> {
    cv_core::emit::ThinkingMode::parse(s).ok_or_else(|| {
        let all = cv_core::emit::ThinkingMode::ALL
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!("unknown --thinking mode {s:?} (expected one of: {all})")
    })
}

/// Group-separate a large integer (`589698` → `589,698`).
pub(crate) fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Humanize a millisecond duration (`3144884` → `52m 24s`).
pub(crate) fn fmt_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_and_duration_formatting() {
        assert_eq!(fmt_int(589698), "589,698");
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(42), "42");
        assert_eq!(fmt_int(1000), "1,000");
        assert_eq!(fmt_duration(3144884), "52m 24s");
        assert_eq!(fmt_duration(5_000), "5s");
        assert_eq!(fmt_duration(7_200_000), "2h 0m");
    }

    #[test]
    fn invalid_read_selectors_and_harness_names_are_usage_errors() {
        for spec in ["10-13", "-5", "5-", "5", "9..3", "1..x", "abc"] {
            assert!(
                parse_range(spec).unwrap_err().downcast_ref::<UsageError>().is_some(),
                "{spec}"
            );
        }
        assert!(parse_harness(&Some("unknown-harness".into()))
            .unwrap_err()
            .downcast_ref::<UsageError>()
            .is_some());
    }

    #[test]
    fn range_grammar_forms() {
        assert_eq!(parse_range("10..13").unwrap(), (10, Some(13)));
        assert_eq!(parse_range("10..").unwrap(), (10, None));
        assert_eq!(parse_range("..13").unwrap(), (0, Some(13)));
        assert_eq!(parse_range(" 2 .. 4 ").unwrap(), (2, Some(4))); // whitespace-tolerant
        assert_eq!(parse_range("0..0").unwrap(), (0, Some(0))); // empty window is legal
    }

    #[test]
    fn range_grammar_rejects_old_forms_with_a_pointer() {
        // The 0.10 forms: each error names the replacement.
        let err = parse_range("10-13").unwrap_err().to_string();
        assert!(err.contains("A..B") && err.contains("--first N"), "{err}");
        let err = parse_range("-5").unwrap_err().to_string();
        assert!(err.contains("--last N"), "{err}");
        let err = parse_range("5-").unwrap_err().to_string();
        assert!(err.contains("A.."), "{err}");
        let err = parse_range("5").unwrap_err().to_string();
        assert!(err.contains("--around 5 --context 0") && err.contains("5..6"), "{err}");
        // Inverted and garbage.
        assert!(parse_range("9..3").unwrap_err().to_string().contains("before start"));
        assert!(parse_range("abc").is_err());
        assert!(parse_range("1..x").is_err());
        assert!(parse_range("").is_err());
    }

    #[test]
    fn window_bounds() {
        let total = || Ok(100);
        assert_eq!(Window::First(7).bounds(total).unwrap(), (0, Some(7)));
        assert_eq!(Window::Last(7).bounds(total).unwrap(), (93, None));
        assert_eq!(Window::Last(500).bounds(total).unwrap(), (0, None)); // more than exist → all
        assert_eq!(Window::Range(3, Some(9)).bounds(total).unwrap(), (3, Some(9)));
        assert_eq!(Window::Range(3, None).bounds(total).unwrap(), (3, None));
        assert_eq!(
            Window::Around { center: 10, context: 5 }.bounds(total).unwrap(),
            (5, Some(16))
        );
        // Around near the start clamps at 0; context 0 is the single message.
        assert_eq!(
            Window::Around { center: 2, context: 5 }.bounds(total).unwrap(),
            (0, Some(8))
        );
        assert_eq!(
            Window::Around { center: 4, context: 0 }.bounds(total).unwrap(),
            (4, Some(5))
        );
        // `--last` is the only selector that pays for a count.
        let never = || -> Result<usize> { panic!("count must not be taken") };
        assert_eq!(Window::First(1).bounds(never).unwrap(), (0, Some(1)));
    }

    #[test]
    fn window_args_selectors_and_clamp() {
        let none = WindowArgs::default();
        assert!(none.window().unwrap().is_none());
        let first = WindowArgs {
            first: Some(3),
            ..Default::default()
        };
        assert_eq!(first.window().unwrap(), Some(Window::First(3)));
        let around = WindowArgs {
            around: Some(9),
            context: 2,
            ..Default::default()
        };
        assert_eq!(around.window().unwrap(), Some(Window::Around { center: 9, context: 2 }));
        let bad = WindowArgs {
            range: Some("1-2".into()),
            ..Default::default()
        };
        assert!(bad.window().is_err());
        assert_eq!(clamp((10, Some(20)), 5), (5, 5)); // past the end → empty, not a panic
        assert_eq!(clamp((2, None), 5), (2, 5));
        assert_eq!(clamp((0, Some(3)), 5), (0, 3));
    }

    #[test]
    fn harness_prefix_splits_only_on_known_names() {
        assert_eq!(split_harness_id("codex:abc", None), (Some(Harness::Codex), "abc"));
        assert_eq!(
            split_harness_id("codex:abc", Some(Harness::Claude)),
            (Some(Harness::Codex), "abc")
        );
        // An unknown head is part of the id; the fallback harness applies.
        assert_eq!(
            split_harness_id("session:abc", Some(Harness::Kimi)),
            (Some(Harness::Kimi), "session:abc")
        );
        assert_eq!(split_harness_id("abc", None), (None, "abc"));
    }

    #[test]
    fn session_row_has_the_contract_keys() {
        let r = SessionRef {
            id: "abc".into(),
            harness: Harness::Claude,
            path: "/x/abc.jsonl".into(),
            cwd: Some("/w".into()),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 3,
        };
        let v = session_row(&r, Some(10));
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "created_at",
                "cwd",
                "harness",
                "id",
                "message_count",
                "path",
                "size_bytes",
                "title",
                "updated_at"
            ]
        );
        assert!(v["title"].is_null(), "missing title is an explicit null");
        assert_eq!(v["size_bytes"], 10);
    }
}
