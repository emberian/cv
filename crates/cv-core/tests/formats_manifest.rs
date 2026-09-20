//! Format manifests vs adapter sources (`docs/INTERFACE-V2.md` §7): every `formats/<harness>.toml`
//! parses, and for every harness the manifest and the adapter agree — each `handled` type appears
//! as a literal in the adapter's source, and each type-like literal in the adapter's match arms is
//! named in the manifest (or its explicit `ignore_literals`). When this fails, the manifest is
//! usually what needs the edit: an adapter learned a type the spec does not list, or the spec
//! promises an arm that was removed.

use cv_core::formats;
use std::path::PathBuf;

fn src_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

#[test]
fn every_harness_has_a_parsing_manifest() {
    let mut bad = Vec::new();
    for (h, m) in formats::manifests() {
        if let Err(e) = m {
            bad.push(format!("{}: {e:#}", h.as_str()));
        }
    }
    assert!(bad.is_empty(), "manifests that fail to parse:\n{}", bad.join("\n"));
}

#[test]
fn adapters_and_manifests_agree() {
    let findings = formats::check_all(&src_root());
    if !findings.is_empty() {
        let mut report = String::new();
        for f in &findings {
            report.push_str(&format!(
                "  {:<14} {:<28} {:<40} {}\n",
                f.harness,
                format!("{:?}", f.kind),
                f.item,
                f.detail
            ));
        }
        panic!("{} manifest/adapter disagreement(s):\n{report}", findings.len());
    }
}
