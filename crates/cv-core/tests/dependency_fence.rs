//! The dependency fence (ramspace lesson): cv-core's dependency set and the purity of the task
//! reducer/model are load-bearing invariants, not habits. The task substrate's whole trust story
//! rests on the reducer being a pure function of the event log — same events in, same model out,
//! on any machine, forever. A dependency that sneaks I/O, network, or process access into the
//! pure modules (or into the crate at large without review) breaks replay determinism silently.
//!
//! Two laws, enforced textually (simple on purpose — a source scan cannot be argued with):
//!
//! 1. **The dependency allowlist.** Every crate named in cv-core's `Cargo.toml` dependency
//!    sections must appear in the literal allowlist below. Adding a dependency is allowed —
//!    by editing the allowlist in the same diff, where a reviewer sees it.
//! 2. **The purity fence.** `src/task/reduce.rs`, `src/task/model.rs`, `src/task/project.rs`,
//!    `src/task/views.rs`, and `src/sanitize.rs` must not touch `std::process`, `std::net`, or
//!    `std::fs`. Reduction, wire types, the projections, and render sanitization are pure; I/O
//!    belongs in `task/store.rs` and the verifier. The fenced set is [`PURE_MODULES`] below —
//!    this paragraph restates it and must be edited with it.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every dependency cv-core is allowed to have, across `[dependencies]` and every
/// `[target.'…'.dependencies]` table. Grow it deliberately, in the same diff as the
/// dependency, never by reflex.
const DEPENDENCY_ALLOWLIST: &[&str] = &[
    "anyhow",
    // Pure-Rust BLAKE3 digests for sealed multi-record packs; deterministic and C-free on wasm32/i686.
    "blake3",
    "chrono",
    "dirs",
    "fs4",
    // Unix only: renamex_np/renameat2 and their errno values for publishing without hard links
    // (src/publish.rs). Never reached by replay or wasm.
    "libc",
    "memchr",
    "memmap2",
    "percent-encoding",
    "rayon",
    "regex",
    "rusqlite",
    "ruzstd",
    "serde",
    "serde_json",
    // Pure-Rust vectorized UTF-8 validation; portable fallback on non-simd wasm32/i686.
    "simdutf8",
    "thiserror",
    "toml",
    // Native synchronous hub replication with verified HTTPS; gated by sqlite, absent on wasm.
    "ureq",
    "uuid",
    "walkdir",
    // Native only: libzstd for compressed sources, already in the shipped binary via tantivy and
    // about 4x ruzstd's decode rate; wasm keeps the pure-Rust ruzstd.
    "zstd",
    // Optional zip-ingest shares the desktop/Web decoder; no default feature or task IO.
    "zip",
];

#[test]
fn dependencies_stay_within_the_allowlist() {
    let manifest_path = manifest_dir().join("Cargo.toml");
    let manifest: toml::Value = fs::read_to_string(&manifest_path)
        .expect("reading cv-core Cargo.toml")
        .parse()
        .expect("parsing cv-core Cargo.toml");

    let mut declared = BTreeSet::new();
    if let Some(deps) = manifest.get("dependencies").and_then(|d| d.as_table()) {
        declared.extend(deps.keys().cloned());
    }
    if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
        for (_cfg, tables) in targets {
            if let Some(deps) = tables.get("dependencies").and_then(|d| d.as_table()) {
                declared.extend(deps.keys().cloned());
            }
        }
    }
    assert!(
        !declared.is_empty(),
        "no dependencies parsed from {} — the fence is scanning the wrong file",
        manifest_path.display()
    );

    let allowed: BTreeSet<&str> = DEPENDENCY_ALLOWLIST.iter().copied().collect();
    let intruders: Vec<&String> = declared.iter().filter(|d| !allowed.contains(d.as_str())).collect();
    assert!(
        intruders.is_empty(),
        "cv-core grew dependencies outside the fence: {intruders:?}.\n\
         The law: cv-core's dependency set is allowlisted because the task substrate's replay \
         determinism (and the wasm build) depend on this crate staying small and pure. If the \
         new dependency is deliberate, add it to DEPENDENCY_ALLOWLIST in tests/dependency_fence.rs \
         in the same diff, so the addition is reviewed as the policy change it is."
    );
}

/// Modules that must stay pure: no process spawning, no network, no filesystem.
const PURE_MODULES: &[&str] = &[
    "src/task/reduce.rs",
    "src/task/model.rs",
    "src/task/project.rs",
    "src/task/views.rs",
    "src/sanitize.rs",
];
const FORBIDDEN: &[&str] = &["std::process", "std::net", "std::fs"];

#[test]
fn pure_modules_touch_no_process_net_or_fs() {
    let mut violations = Vec::new();
    for module in PURE_MODULES {
        let path = manifest_dir().join(module);
        let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        for (idx, line) in source.lines().enumerate() {
            // The fence reads code, not prose: a comment naming a forbidden path (a "must not
            // touch std::fs" in the module's own doc) is documentation, not a touch.
            if line.trim_start().starts_with("//") {
                continue;
            }
            for needle in FORBIDDEN {
                if line.contains(needle) {
                    violations.push(format!("{module}:{}: {}", idx + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "purity fence breached:\n  {}\n\
         The law: the task reducer, its wire types, and render sanitization are PURE — the same \
         event log must reduce to the same model on every machine, forever, and sanitize must be \
         a function of its input string. std::process/std::net/std::fs in these modules makes \
         replay (or rendering) depend on ambient machine state. Do the I/O in task/store.rs or \
         task/verify.rs and pass values in.",
        violations.join("\n  ")
    );
}
