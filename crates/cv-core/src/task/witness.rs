//! Completion evidence resolved from recorded observations.
use super::{EvidenceRef, TaskProjection, TaskState};
use serde::Serialize;

#[derive(Debug, Default, Serialize)]
pub struct Witness {
    pub self_reported: bool,
    pub observed_outcome: Vec<Outcome>,
    /// Receipt links verified against their local capture-log binding.
    pub chain_anchored: Vec<String>,
    pub red_green_contrast: bool,
    pub divergence: Vec<Divergence>,
}
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub event: String,
    pub command_key: String,
    pub exit_code: i64,
}
#[derive(Debug, Serialize)]
pub struct Divergence {
    pub note: String,
    pub receipt: String,
    pub exit_code: i64,
}

struct Recorded {
    event: String,
    session: String,
    seq: u64,
    anchored: bool,
    outcome: Option<Outcome>,
}

#[derive(Default)]
struct Ledger {
    #[cfg(feature = "sqlite")]
    chains: std::collections::HashMap<String, (Vec<crate::state::ReceiptRecord>, bool)>,
}
impl Ledger {
    #[cfg(feature = "sqlite")]
    fn recorded(&mut self, reference: &EvidenceRef) -> Option<Recorded> {
        let EvidenceRef::Receipt { event, digest } = reference else {
            return None;
        };
        let (harness, rest) = event.strip_prefix("receipt:")?.split_once(':')?;
        let (session, seq) = rest.rsplit_once(':')?;
        let seq = seq.parse::<u64>().ok()?;
        let store = crate::state::StateStore::default_store().ok()?;
        let (chain, intact) = match self.chains.entry(session.to_owned()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let chain = store.receipts_for_session(session).ok()?;
                let intact = crate::state::tables::verify_receipt_records(session, &chain).is_ok();
                e.insert((chain, intact))
            }
        };
        if !*intact {
            return None;
        }
        let r = chain.iter().find(|r| r.seq == seq)?;
        let mut body = r.body.clone();
        let h = body.as_object_mut()?.remove("h")?;
        if r.seq != seq
            || r.harness != harness
            || r.receipt_hash != *digest
            || h.as_str()? != digest
            || body["session"].as_str()? != session
            || crate::state::tables::compute_receipt_hash(&r.prev_hash, &body) != *digest
        {
            return None;
        }
        let anchored = store
            .get_operation_binding(event)
            .ok()
            .flatten()
            .is_some_and(|(hash, _)| hash == *digest)
            && *intact;
        let outcome = (|| {
            if body["kind"] != "post" {
                return None;
            }
            Some(Outcome {
                event: event.clone(),
                command_key: body["command_key"].as_str()?.into(),
                exit_code: body["exit_code"].as_i64()?,
            })
        })();
        Some(Recorded {
            event: event.clone(),
            session: format!("{harness}:{session}"),
            seq,
            anchored,
            outcome,
        })
    }
    #[cfg(not(feature = "sqlite"))]
    fn recorded(&mut self, _: &EvidenceRef) -> Option<Recorded> {
        None
    }

    /// The claimant session's own validation runs, from an intact chain only.
    #[cfg(feature = "sqlite")]
    fn validation_runs(&mut self, claimant: &str) -> Vec<Recorded> {
        let Some((harness, session)) = claimant_session(claimant) else {
            return vec![];
        };
        let Ok(store) = crate::state::StateStore::default_store() else {
            return vec![];
        };
        let (chain, intact) = self.chains.entry(session.to_owned()).or_insert_with(|| {
            let chain = store.receipts_for_session(session).unwrap_or_default();
            let intact = crate::state::tables::verify_receipt_records(session, &chain).is_ok();
            (chain, intact)
        });
        if !*intact {
            return vec![];
        }
        chain
            .iter()
            .filter(|r| harness.is_none_or(|h| r.harness == h))
            .filter(|r| r.body["kind"] == "post" && r.body["validation"] == true)
            .filter_map(|r| {
                Some(Recorded {
                    event: format!("receipt:{}:{session}:{}", r.harness, r.seq),
                    session: format!("{}:{session}", r.harness),
                    seq: r.seq,
                    anchored: false,
                    outcome: Some(Outcome {
                        event: format!("receipt:{}:{session}:{}", r.harness, r.seq),
                        command_key: r.body["command_key"].as_str()?.into(),
                        exit_code: r.body["exit_code"].as_i64()?,
                    }),
                })
            })
            .collect()
    }
    #[cfg(not(feature = "sqlite"))]
    fn validation_runs(&mut self, _: &str) -> Vec<Recorded> {
        vec![]
    }
}

/// `(harness, native session)` named by a note's session pointer or actor: `harness:id`,
/// optionally `agent:`-prefixed and carrying a profile-qualified `@<digest>` suffix.
#[cfg(feature = "sqlite")]
fn claimant_session(hint: &str) -> Option<(Option<&str>, &str)> {
    let hint = hint.trim();
    if let Some(receipt) = hint.strip_prefix("receipt:") {
        let event = receipt.split_once('@').map_or(receipt, |(event, _)| event);
        let (harness, rest) = event.split_once(':')?;
        let (session, _) = rest.rsplit_once(':')?;
        return crate::Harness::parse(harness)
            .is_some()
            .then_some((Some(harness), session));
    }
    let hint = hint.strip_prefix("agent:").unwrap_or(hint);
    let (harness, id) = match hint.split_once(':') {
        Some((h, rest)) if crate::Harness::parse(h).is_some() => (Some(h), rest),
        Some(_) => return None,
        None => (None, hint),
    };
    let id = crate::SessionKey::split_selector(id).map_or(id, |(native, _)| native);
    (!id.is_empty() && !matches!(id, "cv" | "agent")).then_some((harness, id))
}

/// A handoff can also cite the receipt inline as `receipt:<harness>:<session>:<seq>@<hash>`.
fn diary_refs(text: &str) -> impl Iterator<Item = EvidenceRef> + '_ {
    text.split_whitespace().filter_map(|word| {
        let (event, digest) = word
            .trim_matches(|c: char| matches!(c, '`' | '[' | ']' | '(' | ')' | '.' | ','))
            .split_once('@')?;
        (event.starts_with("receipt:") && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())).then(
            || EvidenceRef::Receipt {
                event: event.into(),
                digest: digest.into(),
            },
        )
    })
}

/// Structured completion claims (`Done` state or an explicit `Claim` envelope) are checked
/// strictly against verified receipt chains and command exit codes, never prose regexes.
pub fn completion(t: &TaskProjection) -> Witness {
    let mut w = Witness {
        self_reported: t.state == TaskState::Done,
        ..Default::default()
    };
    let mut outcomes = std::collections::BTreeMap::new();
    let mut ledger = Ledger::default();
    // Each source: (note, text, cited evidence, session pointer, claimant, structured_claim).
    let sources = (t.state == TaskState::Done)
        .then_some((
            "done",
            t.done_observed.as_deref().unwrap_or(""),
            t.evidence.as_slice(),
            None,
            t.done_by.as_deref(),
            true,
        ))
        .into_iter()
        .chain(t.notes.iter().map(|n| {
            (
                n.event_id.as_str(),
                n.text.as_str(),
                n.claim.as_ref().map_or(&[][..], |c| c.evidence.as_slice()),
                n.session_ref.as_deref(),
                Some(n.session_ref.as_deref().unwrap_or(&n.by)),
                n.claim.is_some(),
            )
        }))
        .chain(t.status.iter().map(|s| {
            (
                s.event_id.as_str(),
                s.text.as_str(),
                &[][..],
                None,
                Some(s.by.as_str()),
                false,
            )
        }));
    for (note, text, evidence, pointer, claimant, structured_claim) in sources {
        let mut latest = std::collections::BTreeMap::new();
        let cited = diary_refs(text)
            .chain(pointer.into_iter().flat_map(diary_refs))
            .collect::<Vec<_>>();
        // The claimant's own latest validation run per command stands beside what it cites, so
        // an uncited later failure still contradicts a completion claim.
        let own = claimant.map(|c| ledger.validation_runs(c)).unwrap_or_default();
        for recorded in evidence
            .iter()
            .chain(&cited)
            .filter_map(|reference| ledger.recorded(reference))
            .chain(own)
        {
            let Recorded {
                event,
                session,
                seq,
                anchored,
                outcome,
            } = recorded;
            if anchored && !w.chain_anchored.contains(&event) {
                w.chain_anchored.push(event.clone())
            }
            if let Some(run) = outcome {
                let last = latest.entry((session.clone(), run.command_key.clone())).or_insert((
                    seq,
                    event.clone(),
                    run.exit_code,
                ));
                if seq > last.0 {
                    *last = (seq, event, run.exit_code);
                }
                outcomes.insert((session, seq), run);
            }
        }
        if structured_claim {
            for (_, receipt, exit_code) in latest.into_values().filter(|v| v.2 != 0) {
                w.divergence.push(Divergence {
                    note: note.into(),
                    receipt,
                    exit_code,
                });
            }
        }
    }
    let mut failed = std::collections::HashSet::new();
    for ((session, _), run) in outcomes {
        let key = (session, run.command_key.clone());
        if run.exit_code != 0 {
            failed.insert(key);
        } else if failed.contains(&key) {
            w.red_green_contrast = true;
        }
        w.observed_outcome.push(run);
    }
    w
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::task::{new_event, opened_kind, TaskEventKind, TaskReducer};
    #[test]
    fn task_witness_uses_anchored_observations_in_ledger_order() {
        // #tooth Reverse the citations and insert a forged digest. Only the recorded runs count.
        let _world = crate::test_world("task-witness");
        let state = crate::state::StateStore::default_store().unwrap();
        let receipt = |code: i32| {
            let body = serde_json::json!({"kind":"post","tool":"Bash","command_key":"same-command","exit_code":code});
            let (r, _) = state
                .append_receipt_with_inbox("test", "claude", body.as_object().unwrap().clone())
                .unwrap();
            EvidenceRef::Receipt {
                event: format!("receipt:claude:test:{}", r.seq),
                digest: r.receipt_hash,
            }
        };
        let red = receipt(1);
        let green = receipt(0);
        let bad = EvidenceRef::Receipt {
            event: "receipt:claude:test:2".into(),
            digest: "verified".into(),
        };
        let open = new_event(None, "me", opened_kind("test", None));
        let note = new_event(
            Some(&open.id),
            "me",
            TaskEventKind::Noted {
                text: "tests passed".into(),
                session_ref: None,
                claim: Some(serde_json::from_value(serde_json::json!({"kind":"fact","evidence":[red]})).unwrap()),
            },
        );
        let done = new_event(
            Some(&open.id),
            "me",
            TaskEventKind::Done {
                observed: Some("verified".into()),
                check: None,
                evidence: vec![green, red, bad],
                disclosure: None,
            },
        );
        let m = TaskReducer::reduce(&[open.clone(), note.clone(), done]).unwrap();
        let result = completion(&m.tasks[&open.id]);
        assert!(result.self_reported && result.red_green_contrast);
        assert_eq!(result.chain_anchored.len(), 2);
        assert_eq!(
            result.observed_outcome.iter().map(|r| r.exit_code).collect::<Vec<_>>(),
            [1, 0]
        );
        assert_eq!(result.divergence.len(), 1);
        assert_eq!(result.divergence[0].note, note.id);

        // #tooth Divergence depends strictly on structured claims (`claim` / `Done`) and receipt
        // exit codes, never English prose wording.
        let (fail_101, fail_cite) = {
            let body = serde_json::json!({"kind":"post","tool":"Bash","command_key":"cargo-test","exit_code":101});
            let (r, _) = state
                .append_receipt_with_inbox("test2", "claude", body.as_object().unwrap().clone())
                .unwrap();
            let ev = format!("receipt:claude:test2:{}", r.seq);
            let cite = format!("{ev}@{}", r.receipt_hash);
            (
                EvidenceRef::Receipt {
                    event: ev,
                    digest: r.receipt_hash,
                },
                cite,
            )
        };
        let o = new_event(None, "me", opened_kind("check", None));
        let claimed_fail = new_event(
            Some(&o.id),
            "me",
            TaskEventKind::Noted {
                text: "status text ignored".into(),
                session_ref: None,
                claim: Some(serde_json::from_value(serde_json::json!({"kind":"fact","evidence":[fail_101]})).unwrap()),
            },
        );
        let unclaimed_note = new_event(
            Some(&o.id),
            "me",
            TaskEventKind::Noted {
                text: format!("diagnostic log {fail_cite}"),
                session_ref: None,
                claim: None,
            },
        );
        let model = TaskReducer::reduce(&[o.clone(), claimed_fail, unclaimed_note]).unwrap();
        let w2 = completion(&model.tasks[&o.id]);
        assert_eq!(w2.divergence.len(), 1);
        assert_eq!(w2.observed_outcome.len(), 1);
        assert_eq!(w2.observed_outcome[0].exit_code, 101);
    }

    #[test]
    fn task_witness_reads_the_claimants_latest_validation_run() {
        // #tooth A tokenless report is read against the claimant's own latest run per command:
        // a later red contradicts it, an old red followed by green is contrast, not divergence.
        let _world = crate::test_world("task-witness-latest");
        let state = crate::state::StateStore::default_store().unwrap();
        let run = |session: &str, code: i32| {
            let body = serde_json::json!({"kind":"post","tool":"Bash","command_key":"cargo-test",
                "exit_code":code,"validation":true});
            state
                .append_receipt_with_inbox(session, "claude", body.as_object().unwrap().clone())
                .unwrap()
                .0
        };
        let report = |by: &str| {
            let open = new_event(None, "lead", opened_kind("ship", None));
            let done = new_event(
                Some(&open.id),
                by,
                TaskEventKind::Done {
                    observed: Some("all tests passed".into()),
                    check: None,
                    evidence: vec![],
                    disclosure: None,
                },
            );
            let model = TaskReducer::reduce(&[open.clone(), done]).unwrap();
            completion(&model.tasks[&open.id])
        };
        run("regressed", 0);
        let red = run("regressed", 1);
        let qualified = format!("claude:regressed@{}", "a".repeat(64));
        for by in ["claude:regressed", qualified.as_str()] {
            let w = report(by);
            assert_eq!(w.divergence.len(), 1, "{by}");
            assert_eq!(w.divergence[0].receipt, format!("receipt:claude:regressed:{}", red.seq));
        }
        run("repaired", 1);
        run("repaired", 0);
        let w = report("claude:repaired");
        assert!(w.divergence.is_empty() && w.red_green_contrast);

        // A tampered chain is not evidence: nothing from it is read.
        state
            .open_writable()
            .unwrap()
            .execute(
                "UPDATE receipts SET body_json = replace(body_json, '\"exit_code\":1', '\"exit_code\":0')
                 WHERE session_id = 'regressed'",
                [],
            )
            .unwrap();
        let w = report("claude:regressed");
        assert!(w.divergence.is_empty() && w.observed_outcome.is_empty());
    }
}
