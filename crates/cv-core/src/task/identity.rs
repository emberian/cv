//! TOFU per-endpoint tokens: authenticate the claim "I am endpoint X" without any authority
//! machinery (no seats, no roles, no expiry — law 3 stays intact).
//!
//! The [`crate::task::model::TaskEvent::by`] field is a self-asserted string: anyone who can write
//! the log can stamp an event as any endpoint. This module closes that hole for endpoints that opt
//! in, and leaves solo/human use untouched:
//!
//! - **Bind on first use (TOFU).** An endpoint with no binding yet may present a token on an
//!   identity-bearing event; the first token seen becomes its binding (`endpoint -> sha256(token)`,
//!   stored in `$CLUSTERVISION_HOME/tasks/endpoints.json`). The raw token is never written to disk.
//! - **Enforce after binding.** Once an endpoint is bound, every identity-bearing event stamped as
//!   that endpoint must present the matching token or the append is rejected at the store seam.
//! - **Unbound stays trusted.** An endpoint that never presented a token keeps working with no
//!   token at all — the solo CLI / human path is unchanged (backward compatible). The hole is only
//!   closed for those who opt in; a fleet spawner mints a `CV_TOKEN` per worker to opt them in.
//!
//! Scope: this is **authentication** of the `by` claim, not authorization of what an endpoint may
//! do. Only identity-bearing events (claim/release/propose/pass/refute — see
//! [`crate::task::model::TaskEventKind::is_identity_bearing`]) are gated; bookkeeping verbs
//! (open/note/done/abandon) stay token-optional by design. Binding also only happens on
//! identity-bearing events, so a token on a bookkeeping verb is inert. Rotation is a rebind
//! (delete the endpoint's line from `endpoints.json`); a managed rotate/expiry flow is future work.
//!
//! I/O lives here (reading/writing the sidecar); the hashing is pure. This module is deliberately
//! kept out of the purity fence (`tests/dependency_fence.rs`) precisely because the sidecar read is
//! I/O — the reducer/model/stats stay pure and never see a token.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The per-endpoint binding sidecar, next to `events.jsonl` under the same flock.
const BINDINGS_FILE: &str = "endpoints.json";

/// Durable map `endpoint -> sha256(token)`. Versioned like the event log so a newer shape refuses
/// rather than silently mis-reading; `version` defaults to 1 for the current format.
#[derive(Debug, Serialize, Deserialize)]
struct Bindings {
    #[serde(default = "one")]
    version: u64,
    /// `endpoint -> lowercase-hex sha256 of the bound token`. BTreeMap for deterministic on-disk
    /// key order (stable diffs, reproducible bytes).
    #[serde(default)]
    endpoints: BTreeMap<String, String>,
}

fn one() -> u64 {
    1
}

impl Default for Bindings {
    fn default() -> Bindings {
        Bindings {
            version: 1,
            endpoints: BTreeMap::new(),
        }
    }
}

impl Bindings {
    /// Load the sidecar for a task dir. A missing file is an empty binding set (no endpoint bound
    /// yet — the fresh-fleet / solo case). A parse failure is loud: the file exists but is garbage,
    /// and silently treating that as "unbound" would reopen the hole for every bound endpoint.
    fn load(dir: &Path) -> Result<Bindings> {
        let path = dir.join(BINDINGS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let b: Bindings = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing endpoint bindings {}", path.display()))?;
                if b.version != 1 {
                    bail!(
                        "endpoint bindings {} declare version {} (this cv understands 1) — upgrade cv before writing",
                        path.display(),
                        b.version
                    );
                }
                Ok(b)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Bindings::default()),
            Err(e) => Err(e).with_context(|| format!("reading endpoint bindings {}", path.display())),
        }
    }

    /// Persist the sidecar (pretty for human inspection; it is a small, human-facing trust record).
    fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(BINDINGS_FILE);
        let mut json = serde_json::to_string_pretty(self).context("serializing endpoint bindings")?;
        json.push('\n');
        std::fs::write(&path, json).with_context(|| format!("writing endpoint bindings {}", path.display()))?;
        Ok(())
    }
}

/// What the store should do with an about-to-be-written event, once its identity is checked.
#[derive(Debug, PartialEq)]
pub(crate) enum Decision {
    /// No token machinery applies (non-identity event, or an unbound endpoint with no token, or a
    /// bound endpoint whose presented token matched): write the event unchanged.
    Proceed,
    /// Trust-on-first-use: no binding existed and a token was presented — persist this binding,
    /// then write the event.
    Bind { endpoint: String, hash: String },
}

/// Decide whether an event stamped `by` may be written, given whatever token the caller presented.
///
/// `identity_bearing` is [`crate::task::model::TaskEventKind::is_identity_bearing`] for the event's
/// kind; a `false` here means the token is irrelevant and the event proceeds untouched.
///
/// Called under the store's `events.lock`, so the load-decide-(bind) sequence is atomic with the
/// append that follows: a `Bind` decision's [`commit`] runs before the event line is written, both
/// under the same lock.
pub(crate) fn authorize(dir: &Path, by: &str, identity_bearing: bool, token: Option<&str>) -> Result<Decision> {
    if !identity_bearing {
        return Ok(Decision::Proceed);
    }
    let bindings = Bindings::load(dir)?;
    match (bindings.endpoints.get(by), token) {
        // Bound + token presented: authenticate against the first-use binding.
        (Some(expected), Some(tok)) => {
            if &token_hash(tok) == expected {
                Ok(Decision::Proceed)
            } else {
                bail!(
                    "identity rejected: this append is stamped `by: {by}` but the presented token does not match \
                     the token {by} bound on first use — refusing a possible impersonation"
                )
            }
        }
        // Bound + no token: the endpoint opted in, so a tokenless append as it is impersonation.
        (Some(_), None) => bail!(
            "identity rejected: endpoint `{by}` is token-bound — present its token via CV_TOKEN or --token to \
             append identity-bearing events as `{by}`"
        ),
        // Unbound + token: trust on first use, bind it.
        (None, Some(tok)) => Ok(Decision::Bind {
            endpoint: by.to_string(),
            hash: token_hash(tok),
        }),
        // Unbound + no token: trusted by design (solo CLI / human / not-yet-opted-in fleet worker).
        (None, None) => Ok(Decision::Proceed),
    }
}

/// Persist a first-use binding. Idempotent for a re-seen endpoint/hash pair; called only for a
/// [`Decision::Bind`], under the store lock, immediately before the event is appended.
pub(crate) fn commit(dir: &Path, endpoint: &str, hash: &str) -> Result<()> {
    let mut bindings = Bindings::load(dir)?;
    bindings.endpoints.insert(endpoint.to_string(), hash.to_string());
    bindings.save(dir)
}

/// The bindings sidecar path for a task dir (test helper).
#[cfg(test)]
fn bindings_path(dir: &Path) -> std::path::PathBuf {
    dir.join(BINDINGS_FILE)
}

/// Lowercase-hex SHA-256 of `input`. Used to hash tokens before they touch disk (the raw token is
/// never stored). Pure: a function of its bytes only. The digest itself is [`crate::digest`]'s.
pub(crate) fn token_hash(input: &str) -> String {
    crate::digest::sha256_hex(input.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4 known-answer vectors — pin the vendored digest against the standard so it can
    /// never silently drift into a wrong (but stable) hash.
    #[test]
    fn sha256_matches_fips_vectors() {
        assert_eq!(
            token_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // The classic 448-bit (56-byte) message — exercises the two-block padding path.
        assert_eq!(
            token_hash("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn unbound_no_token_proceeds_and_binds_nothing() {
        let dir = tmp_dir();
        assert_eq!(authorize(&dir, "agent:solo", true, None).unwrap(), Decision::Proceed);
        assert!(!bindings_path(&dir).exists(), "no token → no sidecar written");
    }

    #[test]
    fn first_use_binds_then_matching_token_authenticates_and_wrong_token_rejects() {
        let dir = tmp_dir();
        // First use with a token → Bind.
        let Decision::Bind { endpoint, hash } = authorize(&dir, "agent:w", true, Some("s3cret")).unwrap() else {
            panic!("first use with a token should bind");
        };
        assert_eq!(endpoint, "agent:w");
        assert_eq!(hash, token_hash("s3cret"));
        commit(&dir, &endpoint, &hash).unwrap();

        // Bound + matching token → Proceed.
        assert_eq!(
            authorize(&dir, "agent:w", true, Some("s3cret")).unwrap(),
            Decision::Proceed
        );
        // Bound + wrong token → reject.
        let err = authorize(&dir, "agent:w", true, Some("guess")).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        // Bound + no token → reject (opted-in endpoint must always present its token).
        let err = authorize(&dir, "agent:w", true, None).unwrap_err();
        assert!(err.to_string().contains("token-bound"), "{err}");
    }

    #[test]
    fn non_identity_events_ignore_tokens_entirely() {
        let dir = tmp_dir();
        // Bind agent:w.
        commit(&dir, "agent:w", &token_hash("s3cret")).unwrap();
        // A non-identity event stamped by the bound endpoint, no token: still proceeds (bookkeeping
        // verbs are token-optional by design), and a wrong token on a non-identity event is inert.
        assert_eq!(authorize(&dir, "agent:w", false, None).unwrap(), Decision::Proceed);
        assert_eq!(
            authorize(&dir, "agent:w", false, Some("wrong")).unwrap(),
            Decision::Proceed
        );
    }

    #[test]
    fn raw_token_never_touches_disk() {
        let dir = tmp_dir();
        commit(&dir, "agent:w", &token_hash("super-secret-token")).unwrap();
        let on_disk = std::fs::read_to_string(bindings_path(&dir)).unwrap();
        assert!(
            !on_disk.contains("super-secret-token"),
            "raw token must not be stored: {on_disk}"
        );
        assert!(
            on_disk.contains(&token_hash("super-secret-token")),
            "hash is what is stored"
        );
    }

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cv-identity-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
