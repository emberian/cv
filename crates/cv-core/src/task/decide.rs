//! Reading decisions out of prose: the `DECIDE (…): …` note convention an orchestrator falls into
//! when it has no decision kind, parsed into a title, a default and the alternatives so
//! `cv task split` can turn each such note into a real decision task. Pure text; no I/O.
//!
//! The shapes this was written against (one day's notes on one task, verbatim forms):
//!
//! - `DECIDE: move the build base from /tank (…) to NVMe? … Default if silent: keep /tank.`
//! - `DECIDE (P6, default stands if silent): WHO MAY REFILL … Recommend: keep open (…).`
//! - `DECIDE (K-PORTAL, default stands if silent): who births … — default = X; alternative = Y.`
//! - `DECIDE (N11, default stands if silent): … Default = constants; a door that needs time …`
//! - `DECIDE (K-DOC-HISTORY, defaults stand if silent): (a) … Default = … (b) … Default = keep; …`
//! - `DECIDE (DEOS.md §5, defaults stand if silent; the doc has the full argument): (1) … (7) …`
//!
//! A note that merely *mentions* `DECIDE` mid-text is not a decision note (the convention is
//! leading); callers may count those and say so.

use chrono::{DateTime, Utc};

/// What one `DECIDE` note says, structurally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecideNote {
    /// The label inside the parenthetical with the boilerplate removed (`P6`, `K-PORTAL`,
    /// `DEOS.md §5`), when there was one.
    pub label: Option<String>,
    /// One line: `<label>: <first clause of the question>`, parentheticals dropped, ≤ 90 chars.
    pub title: String,
    /// The text after a `Default =` / `Default if silent:` / `Recommend:` marker (every such
    /// clause, joined with ` · ` when there are several). Empty when the note names none — the
    /// caller substitutes [`AS_PROPOSED`], since "defaults stand if silent" makes the note's own
    /// proposal the default.
    pub default_choice: String,
    /// The alternatives the note names (`alternative = X`, `the alternative is X`), in order.
    pub alternatives: Vec<String>,
}

/// The default recorded for a decision note that states its proposal but no explicit default:
/// accepting it means "do what the note says".
pub const AS_PROPOSED: &str = "as proposed";

/// Is this note a decision note (leading `DECIDE`)?
pub fn is_decide_note(text: &str) -> bool {
    text.trim_start().starts_with("DECIDE")
}

/// Does the note mention `DECIDE` after its first sentence (a decision buried in a report)?
pub fn mentions_decide_mid_text(text: &str) -> bool {
    let t = text.trim_start();
    !t.starts_with("DECIDE") && t.contains("DECIDE")
}

/// Parse a leading-`DECIDE` note. `None` when the note does not start with `DECIDE`.
pub fn parse_decide_note(text: &str) -> Option<DecideNote> {
    let t = text.trim();
    let rest = t.strip_prefix("DECIDE")?;
    let mut rest = rest.trim_start();

    // Optional parenthetical head: `(P6, default stands if silent; …)`.
    let mut label = None;
    if let Some(inner) = rest.strip_prefix('(') {
        if let Some(close) = matching_paren(inner) {
            let head = &inner[..close];
            label = label_of(head);
            rest = inner[close + 1..].trim_start();
        }
    }
    // `DECIDE later, when X:` — a qualifier before the colon is part of the question, keep it.
    let body = match rest.find(':') {
        Some(i) if rest[..i].trim().is_empty() || rest[..i].len() < 40 => {
            let qualifier = rest[..i].trim();
            let after = rest[i + 1..].trim();
            if qualifier.is_empty() {
                after.to_string()
            } else {
                format!("{qualifier}: {after}")
            }
        }
        _ => rest.to_string(),
    };

    let clause = first_clause(&strip_parentheticals(&body));
    let clause = cap(&clause, 90);
    let title = match &label {
        Some(l) if !clause.is_empty() => format!("{l}: {clause}"),
        Some(l) => l.clone(),
        None if clause.is_empty() => "DECIDE".to_string(),
        None => clause,
    };

    let defaults = clauses_after(&body, DEFAULT_MARKERS);
    let alternatives = clauses_after(&body, ALTERNATIVE_MARKERS);
    Some(DecideNote {
        label,
        title,
        default_choice: defaults.join(" · "),
        alternatives,
    })
}

/// The option list a parsed note poses: the default (or [`AS_PROPOSED`]) first, then every
/// alternative, deduplicated.
pub fn options_of(note: &DecideNote) -> (String, Vec<String>) {
    let default = if note.default_choice.is_empty() {
        AS_PROPOSED.to_string()
    } else {
        note.default_choice.clone()
    };
    let mut options = vec![default.clone()];
    for a in &note.alternatives {
        if !options.iter().any(|o| o == a) {
            options.push(a.clone());
        }
    }
    (default, options)
}

/// The markers whose following clause is the default. Matched case-insensitively at a word
/// boundary; the clause runs to the end of the sentence (see [`clause_end`]).
const DEFAULT_MARKERS: &[&str] = &[
    "default if silent:",
    "default if silent =",
    "defaults if silent:",
    "default =",
    "defaults =",
    "default:",
    "defaults:",
    "recommendation:",
    "recommended:",
    "recommend:",
];

const ALTERNATIVE_MARKERS: &[&str] = &[
    "the alternative is",
    "the alternatives are",
    "alternative =",
    "alternatives =",
    "alternative:",
    "alternatives:",
];

/// Every clause that follows one of `markers` in `text`, in text order.
fn clauses_after(text: &str, markers: &[&str]) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut hits: Vec<(usize, usize)> = Vec::new(); // (start of marker, end of marker)
    for m in markers {
        let mut from = 0;
        while let Some(i) = lower[from..].find(m) {
            let at = from + i;
            let boundary = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
            if boundary {
                hits.push((at, at + m.len()));
            }
            from = at + m.len();
        }
    }
    hits.sort();
    // A longer marker that starts where a shorter one does wins (`default if silent:` over
    // `default:` cannot collide, but `recommend:`/`recommendation:` share a prefix).
    hits.dedup_by(|b, a| {
        b.0 == a.0 && {
            a.1 = a.1.max(b.1);
            true
        }
    });
    hits.into_iter()
        .map(|(_, end)| {
            let tail = &text[end..];
            let tail = tail.trim_start();
            let stop = clause_end(tail);
            tail[..stop].trim().trim_end_matches(['.', ';', ',']).trim().to_string()
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// Where a default/alternative clause ends: the first sentence end at paren depth 0 (`. `, `? `,
/// `! `, end of text), or a `; alternative`/`; the alternative` turn — a `;` alone does not end
/// it (`keep; a door that needs time reads the clock` is one stance).
fn clause_end(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let lower = s.to_lowercase();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'.' | b'?' | b'!' if depth <= 0 => {
                let next = bytes.get(i + 1).copied();
                if next.is_none() || next == Some(b' ') || next == Some(b'\n') {
                    // `e.g.`/`i.e.`/a number like `2.5` are not sentence ends when the next
                    // byte is not whitespace — handled by the `next` check above.
                    return i;
                }
            }
            b';' if depth <= 0 => {
                let after = lower[i + 1..].trim_start();
                if after.starts_with("alternative") || after.starts_with("the alternative") {
                    return i;
                }
            }
            b'\n' if depth <= 0 => return i,
            _ => {}
        }
        i += 1;
    }
    s.len()
}

/// The label in a parenthetical head: its first `,`/`;` segment that is not the
/// "default(s) stand(s) if silent / until vetoed" boilerplate.
fn label_of(head: &str) -> Option<String> {
    head.split([',', ';'])
        .map(str::trim)
        .find(|seg| !seg.is_empty() && !is_boilerplate(seg))
        .map(str::to_string)
}

fn is_boilerplate(seg: &str) -> bool {
    let l = seg.to_lowercase();
    (l.starts_with("default") && (l.contains("stand") || l.contains("if silent") || l.contains("until")))
        || l.starts_with("the doc has")
        || l.starts_with("see ")
}

/// Index of the `)` matching an already-consumed `(`, depth-aware.
fn matching_paren(s: &str) -> Option<usize> {
    let mut depth = 1i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Drop every `(…)` group (nested ones included) and tidy the spacing.
pub fn strip_parentheticals(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let mut tidy = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        let sp = c == ' ';
        if !(sp && prev_space) {
            tidy.push(c);
        }
        prev_space = sp;
    }
    tidy.replace(" ?", "?")
        .replace(" .", ".")
        .replace(" ,", ",")
        .trim()
        .to_string()
}

/// The first clause of a question: up to the first sentence end, `; `, ` — ` or ` -- `.
fn first_clause(s: &str) -> String {
    let s = s.trim();
    let mut end = s.len();
    for pat in [". ", "? ", "! ", "; ", " — ", " -- ", "\n"] {
        if let Some(i) = s.find(pat) {
            // Sentence punctuation stays on the title (`…to NVMe /?`); separators do not.
            let keeps_mark = pat.starts_with(['?', '!', '.']);
            end = end.min(if keeps_mark { i + 1 } else { i });
        }
    }
    s[..end].trim().trim_end_matches(['.', ';', ',']).to_string()
}

/// Cap at `max` chars on a word boundary with an ellipsis.
fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(max.saturating_sub(1)).collect();
    if let Some(i) = cut.rfind(' ') {
        if i > max / 2 {
            cut.truncate(i);
        }
    }
    format!("{}…", cut.trim_end_matches([' ', ',', ';', ':']))
}

/// Parse a `--by` deadline: a duration from `now` (`3d`, `48h`, `1w`), a bare date (`2026-10-03`
/// = the end of that local day), a local datetime (`2026-10-03T18:00`), or RFC 3339.
pub fn parse_deadline(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let s = s.trim();
    if let Some(d) = super::project::parse_duration(s) {
        return Ok(now + d);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(t) = Local.from_local_datetime(&ndt).single() {
                return Ok(t.with_timezone(&Utc));
            }
        }
    }
    if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let ndt = nd.and_hms_opt(23, 59, 59).expect("end of day exists");
        if let Some(t) = Local.from_local_datetime(&ndt).single() {
            return Ok(t.with_timezone(&Utc));
        }
    }
    Err(format!(
        "cannot read {s:?} as a deadline: a duration (3d, 48h, 1w), a date (2026-10-03), a datetime \
         (2026-10-03T18:00) or RFC 3339"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TANK: &str = "DECIDE: move the hbox Mini build base from /tank (2×7200 rpm, ~3.4 MB/s under load; cold Mathlib 2,129 s vs 9.6 s on persvati) to NVMe / (144 GB free)? Details on task WARM-BASES 01a0f533. Default if silent: keep /tank, never time on hbox.";
    const P6: &str = "DECIDE (P6, default stands if silent): WHO MAY REFILL a Hermes purse — today anyone burning their OWN credit may fund any purse (the purse law accepts the edge from any subject). Per-refill consent from the purse owner = one extra law condition. Recommend: keep open (gifting credit to a friend's agent is a feature; the refiller only loses their own credit).";
    const PORTAL: &str = "DECIDE (K-PORTAL, default stands if silent): who births a traveller's GUEST cell on the foreign node and pays its birth fee — default = the foreign node's concierge births it under R-guests (MUD §2.11); alternative = the portal receiver allocates it (then F pays for every arrival).";
    const N11: &str = "DECIDE (N11, default stands if silent): a door's poke sample fixes eny/our/now = 0 (constants) so a claim goes stale only when the state or event number moved and concurrent pokes serialise; the alternative (now = block height) makes every door claim stale each block. Default = constants; a door that needs time reads clock/now from the kernel's clock cell instead.";
    const HISTORY: &str = "DECIDE (K-DOC-HISTORY, defaults stand if silent): (a) the Theory's per-document event-log family (HyperdocumentEventLog / VersionEffects / VersionEventRecord) has NO host caller; history is now a proven view over the signed log (sound + complete). Default = do NOT wire a second history; delete the event-log family as a twin unless K-DOC-EVENTS (DEOS Oct 26+, O(doc) history) is wanted. (b) a late joiner sees WHO edited earlier versions and WHEN but not WHAT (content reads are judged by the grant at that height; metadata by current grants). Default = keep; hiding the metadata too is one predicate.";
    const DEOS: &str = "DECIDE (DEOS.md §5, defaults stand if silent; the doc has the full argument): (1) quotes: REFERENCED by default, CARRIED only when the source room's law admits it (one default, no second shape) — subsumes QUOTE-DISCLOSE; (2) web face: LOCAL ONLY in October, no hosted web app (a hosted one = the ssh shell with a skin); (3) the page is READ-ONLY on the 13th, writes on the 26th.";
    const BRAID: &str = "DECIDE (PAY-BRAID, default stands if silent): purse-refill refusals (op 115) are now UNDISCLOSED like pay submits (op 105) because their reasons reveal Book state. Default = keep; the alternative is naming refill reasons to the refiller only (they are the payer, so the leak is about their own account).";

    #[test]
    fn bare_decide_takes_the_question_as_title_and_the_if_silent_default() {
        let n = parse_decide_note(TANK).unwrap();
        assert_eq!(n.label, None);
        assert_eq!(n.title, "move the hbox Mini build base from /tank to NVMe /?");
        assert_eq!(n.default_choice, "keep /tank, never time on hbox");
        assert!(n.alternatives.is_empty());
        let (d, opts) = options_of(&n);
        assert_eq!(d, "keep /tank, never time on hbox");
        assert_eq!(opts, vec!["keep /tank, never time on hbox"]);
    }

    #[test]
    fn labelled_note_with_recommend_marker() {
        let n = parse_decide_note(P6).unwrap();
        assert_eq!(n.label.as_deref(), Some("P6"));
        assert_eq!(n.title, "P6: WHO MAY REFILL a Hermes purse");
        assert_eq!(
            n.default_choice,
            "keep open (gifting credit to a friend's agent is a feature; the refiller only loses their own credit)"
        );
    }

    #[test]
    fn default_and_alternative_markers_become_options() {
        let n = parse_decide_note(PORTAL).unwrap();
        assert_eq!(
            n.title,
            "K-PORTAL: who births a traveller's GUEST cell on the foreign node and pays its birth fee"
        );
        assert_eq!(
            n.default_choice,
            "the foreign node's concierge births it under R-guests (MUD §2.11)"
        );
        assert_eq!(
            n.alternatives,
            vec!["the portal receiver allocates it (then F pays for every arrival)"]
        );
        let (_, opts) = options_of(&n);
        assert_eq!(opts.len(), 2);

        let n = parse_decide_note(BRAID).unwrap();
        assert_eq!(n.default_choice, "keep");
        assert_eq!(
            n.alternatives,
            vec!["naming refill reasons to the refiller only (they are the payer, so the leak is about their own account)"]
        );
    }

    #[test]
    fn a_semicolon_inside_a_stance_does_not_end_it_and_titles_are_capped() {
        let n = parse_decide_note(N11).unwrap();
        assert_eq!(
            n.default_choice,
            "constants; a door that needs time reads clock/now from the kernel's clock cell instead"
        );
        assert!(
            n.title
                .starts_with("N11: a door's poke sample fixes eny/our/now = 0 so a claim"),
            "{}",
            n.title
        );
        assert!(n.title.chars().count() <= 95, "{}", n.title);
        assert!(
            n.alternatives.is_empty(),
            "`the alternative (now = …)` is prose, not a marker"
        );
    }

    #[test]
    fn several_defaults_are_joined_and_no_default_means_as_proposed() {
        let n = parse_decide_note(HISTORY).unwrap();
        assert_eq!(n.label.as_deref(), Some("K-DOC-HISTORY"));
        assert_eq!(
            n.default_choice,
            "do NOT wire a second history; delete the event-log family as a twin unless K-DOC-EVENTS (DEOS Oct 26+, O(doc) history) is wanted · keep; hiding the metadata too is one predicate"
        );
        let n = parse_decide_note(DEOS).unwrap();
        assert_eq!(
            n.label.as_deref(),
            Some("DEOS.md §5"),
            "boilerplate and the doc remark are not the label"
        );
        assert_eq!(n.default_choice, "", "`REFERENCED by default,` is prose, not a marker");
        let (d, opts) = options_of(&n);
        assert_eq!(d, AS_PROPOSED);
        assert_eq!(opts, vec![AS_PROPOSED]);
        assert!(
            n.title.starts_with("DEOS.md §5: quotes: REFERENCED by default"),
            "{}",
            n.title
        );
    }

    #[test]
    fn only_leading_decide_is_a_decision_note() {
        assert!(parse_decide_note("PRIVACY FINDING: … DECIDE later, when openings are built: a key.").is_none());
        assert!(mentions_decide_mid_text(
            "JOIN-SOLANA works. DECIDE: one enrollment per tip, raise the floor or batch."
        ));
        assert!(!mentions_decide_mid_text(TANK));
        assert!(is_decide_note("  DECIDE (X): y"));
        assert!(!is_decide_note("decide: lower-case is a word, not the convention"));
        let n = parse_decide_note("DECIDE (X, default stands if silent)").unwrap();
        assert_eq!(n.title, "X");
    }

    #[test]
    fn deadlines_read_durations_dates_and_datetimes() {
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        assert_eq!(parse_deadline("3d", now).unwrap(), now + chrono::Duration::days(3));
        assert_eq!(
            parse_deadline("2026-10-05T10:00:00Z", now).unwrap(),
            "2026-10-05T10:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        let d = parse_deadline("2026-10-03", now).unwrap();
        assert!(
            d > "2026-10-02T12:00:00Z".parse::<DateTime<Utc>>().unwrap(),
            "end of local day"
        );
        assert!(parse_deadline("whenever", now).is_err());
    }
}
