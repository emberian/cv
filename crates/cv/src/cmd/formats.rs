//! `cv formats` — the format census and the manifest check (docs/INTERFACE-V2.md §7).
//!
//! Two questions about the same thing, from two directions:
//!
//! - `cv formats census` asks **real data**: parse the most recent sessions per harness in
//!   format-complete mode and report what record vocabulary showed up, marking anything the
//!   manifest does not name as `NEW` — that is drift arriving on disk.
//! - `cv formats check` asks **the source**: does each adapter still do what `formats/<h>.toml`
//!   promises, and does the manifest still name every literal the adapter matches on? The same
//!   comparison `tests/formats_manifest.rs` runs in CI, available against any checkout.

use anyhow::Result;
use clap::Subcommand;
use cv_core::formats::{self, Status};
use cv_core::ir::Harness;
use serde_json::json;
use std::path::PathBuf;

use crate::util::parse_harness;

#[derive(Subcommand)]
pub(crate) enum FormatsCmd {
    /// What the most recent sessions actually contain, per harness: message kinds, block types and
    /// the record vocabulary the adapter did not interpret. Types the manifest does not name are
    /// marked `NEW` — the drift signal.
    Census {
        /// Only this harness.
        #[arg(long)]
        harness: Option<String>,
        /// How many recent sessions per harness to parse.
        #[arg(long, default_value_t = 20, value_name = "N")]
        recent: usize,
        #[arg(long)]
        json: bool,
    },
    /// Compare every adapter's source against its manifest, both directions: a `handled` type that
    /// no longer appears in the adapter, and a literal the adapter matches on that the manifest
    /// does not name. Exit 1 if anything disagrees.
    Check {
        /// Adapter source root. Default: the `crates/cv-core/src` of the checkout cv was built in.
        #[arg(long, value_name = "DIR")]
        src: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn cmd_formats(action: FormatsCmd) -> Result<()> {
    match action {
        FormatsCmd::Census { harness, recent, json } => census(parse_harness(&harness)?, recent, json),
        FormatsCmd::Check { src, json } => check(src, json),
    }
}

/// `crates/cv-core/src` of the checkout this binary was built from.
fn default_src_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("cv-core").join("src"))
        .unwrap_or_else(|| PathBuf::from("crates/cv-core/src"))
}

fn check(src: Option<PathBuf>, json_out: bool) -> Result<()> {
    let root = src.unwrap_or_else(default_src_root);
    let findings = formats::check_all(&root);
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "src_root": root,
                "finding_count": findings.len(),
                "findings": findings,
            }))?
        );
    } else if findings.is_empty() {
        println!(
            "{} manifests agree with their adapters ({})",
            formats::MANIFEST_SOURCES.len(),
            root.display()
        );
    } else {
        // A binary installed from a release has no cv checkout beside it, and `--src` defaults to
        // the directory it was BUILT in — so the honest answer there is "point me at a checkout",
        // not a wall of per-harness findings. Say that first, before the list.
        if !root.is_dir() {
            eprintln!("✦ {} is not a directory.", root.display());
            eprintln!("  `formats check` reads the adapter sources, so it needs a cv checkout:");
            eprintln!("  pass `--src <repo>/crates/cv-core/src`.");
            eprintln!("  (`formats census` needs no source — it reads your real sessions.)");
        }
        for f in &findings {
            println!(
                "{:<15} {:<26} {:<34} {}",
                f.harness,
                kind_word(f.kind),
                f.item,
                f.detail
            );
        }
        println!(
            "\n{} disagreement(s) — fix formats/<harness>.toml, or the adapter",
            findings.len()
        );
    }
    if findings.is_empty() {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

fn kind_word(k: formats::FindingKind) -> &'static str {
    match k {
        formats::FindingKind::HandledMissingInSource => "handled_missing_in_source",
        formats::FindingKind::LiteralMissingInManifest => "literal_missing_in_manifest",
        formats::FindingKind::SourceFileMissing => "source_file_missing",
    }
}

fn census(harness: Option<Harness>, recent: usize, json_out: bool) -> Result<()> {
    let c = formats::census(harness, recent);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&c)?);
        return Ok(());
    }
    if c.harnesses.is_empty() {
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
                    "manifest {} handled / {} generic / {} carried / {} ignored",
                    m.count(Status::Handled),
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
                    (false, Some(st)) => format!("{:<4}", &st.as_str()[..3]),
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
