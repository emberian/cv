//! `cv formats` — the format census (docs/INTERFACE-V2.md §7): parse the most recent sessions per
//! harness in format-complete mode and report what record vocabulary showed up, marking anything
//! the manifest does not name as `NEW` — that is drift arriving on disk.
//!
//! The other direction — does each adapter's source still do what `formats/<h>.toml` promises —
//! needs a checkout, so it is a test, not a command: `cargo test -p clustervision-core --test
//! formats_manifest`.

use anyhow::Result;
use cv_core::formats::{self, Status};
use cv_core::ir::Harness;

use crate::util::parse_harness;

pub(crate) fn cmd_formats(harness: Option<String>, recent: usize, json: bool) -> Result<()> {
    census(parse_harness(&harness)?, recent, json)
}

fn census(harness: Option<Harness>, recent: usize, json_out: bool) -> Result<()> {
    let c = formats::census(harness, recent);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&c)?);
        return Ok(());
    }
    if c.harnesses.is_empty() {
        if recent == 0 {
            println!("no sessions sampled (--recent 0)");
            return Ok(());
        }
        println!(
            "no sessions found (try `cv index`{})",
            harness
                .map(|h| format!(" — or another --harness than {}", h.as_str()))
                .unwrap_or_default()
        );
        return Ok(());
    }
    for (name, h) in &c.harnesses {
        let m = Harness::ALL
            .iter()
            .find(|x| x.as_str() == name)
            .and_then(|h| formats::manifest(*h).ok());
        let cover = m
            .as_ref()
            .map(|m| {
                format!(
                    "manifest {} generic / {} carried / {} ignored (handled: the adapter's match arms)",
                    m.count(Status::Generic),
                    m.count(Status::Carried),
                    m.count(Status::Ignored)
                )
            })
            .unwrap_or_else(|| "no manifest".into());
        println!(
            "\n{name}  ({} session(s) of the {recent} most recent, {} parse error(s))",
            h.sessions, h.parse_errors
        );
        println!("  {cover}");
        if let Some(error) = &h.first_error {
            println!("  first parse error: {error}");
        }
        if !h.kinds.is_empty() {
            println!("  kinds:  {}", tally(&h.kinds));
        }
        if !h.block_types.is_empty() {
            println!("  blocks: {}", tally(&h.block_types));
        }
        if h.records.is_empty() {
            println!("  uninterpreted records: none");
        } else {
            println!("  uninterpreted records:");
            for (ty, s) in &h.records {
                let mark = match (s.new, s.status) {
                    (true, _) => "NEW ".to_string(),
                    (false, Some(st)) => format!("{:<4}", st.as_str().chars().take(3).collect::<String>()),
                    (false, None) => "    ".to_string(),
                };
                println!("    {mark} {:<44} {:>6}  {}", ty, s.count, s.first_seen.display());
            }
        }
        if !h.unseen_manifest_types.is_empty() {
            println!(
                "  carried types not seen in this sample: {}",
                h.unseen_manifest_types.join(" ")
            );
        }
    }
    let new = c.new_types();
    if new.is_empty() {
        println!("\nno unknown vocabulary in {} harness(es)", c.harnesses.len());
    } else {
        println!("\n{} type(s) no manifest names:", new.len());
        for (h, t, n) in new {
            println!("  {h:<15} {t:<44} {n}");
        }
    }
    Ok(())
}

fn tally(m: &std::collections::BTreeMap<String, u64>) -> String {
    m.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ")
}
