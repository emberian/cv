//! Falsifiable `#tooth` verification gates for strict ordered-unit equivalence, destination-volume
//! pre-publish staging (`--skills` included), and same-transaction SQLite rollback (`OpenCode`
//! v1/v2, `Hermes`, `OpenClaw` SQLite).

use super::fidelity::{diff_fidelity, reparse_staged_projected, source_projected_with_opts};
use super::stage::StagedSource;
use super::*;
use crate::harness::claude::skills::SkillsPolicy;
use serde_json::json;

fn rich_conversation(harness: Harness) -> Session {
    let mut sys = Message::new(Role::System);
    sys.kind = MessageKind::Notice;
    sys.content.push(Block::from_text("system guardrail alpha".into()));

    let mut u1 = Message::new(Role::User);
    u1.content.push(Block::from_text("inspect alpha and beta".into()));

    let mut a1 = Message::new(Role::Assistant);
    a1.model = Some("claude-sonnet-4-6".into());
    a1.content = vec![
        Block::Thinking {
            text: "plan first step".into(),
            signature: Some("sig-plan-1".into()),
            encrypted: None,
            redacted: false,
            extensions: Default::default(),
        },
        Block::from_text("Running parallel reads.".into()),
        Block::ToolUse {
            id: "call_alpha".into(),
            name: "Read".into(),
            input: json!({
                "alpha": { "count": 42, "path": "/src/alpha.rs" },
                "zeta": [true, -0.0]
            })
            .into(),
            namespace: None,
            extensions: Default::default(),
        },
        Block::ToolUse {
            id: "call_beta".into(),
            name: "Read".into(),
            input: json!({
                "alpha": { "count": 7, "path": "/src/beta.rs" },
                "zeta": [false, 1]
            })
            .into(),
            namespace: None,
            extensions: Default::default(),
        },
    ];

    let mut t1 = Message::new(Role::Tool);
    t1.content = vec![
        Block::ToolResult {
            tool_use_id: "call_alpha".into(),
            content: "alpha payload".into(),
            is_error: false,
            tool_name: Some("Read".into()),
            status: None,
            details: None,
            extensions: Default::default(),
        },
        Block::ToolResult {
            tool_use_id: "call_beta".into(),
            content: "beta payload".into(),
            is_error: true,
            tool_name: Some("Read".into()),
            status: None,
            details: None,
            extensions: Default::default(),
        },
    ];

    let mut u2 = Message::new(Role::User);
    u2.content.push(Block::from_text("summarize both".into()));

    let mut a2 = Message::new(Role::Assistant);
    a2.model = Some("claude-sonnet-4-6".into());
    a2.content.push(Block::from_text("Final synthesis complete.".into()));

    Session {
        cwd: Some(PathBuf::from("/work/strict-tooth")),
        title: Some("strict tooth session".into()),
        created_at: Some("2026-10-01T12:00:00Z".parse().unwrap()),
        updated_at: Some("2026-10-01T12:05:00Z".parse().unwrap()),
        model: Some("claude-sonnet-4-6".into()),
        messages: vec![sys, u1, a1, t1, u2, a2],
        ..Session::new("tooth-orig", harness)
    }
}

/// `#tooth` Gate 1: Single-byte flips in `Block::Text` (System, User, Assistant), single-byte
/// flips in `Block::ToolUse.input`, swapped assistant replies, duplicated turns/blocks, crossed
/// parallel `tool_use_id` ↔ `tool_result` links, collapsed tool IDs, and corrupted `Image`/`File`
/// blocks ALL turn `--strict` RED with human-readable `turn[i].block[j]` diagnostics, while
/// canonical JSON key reordering / `-0.0` normalization and injective `ρ` ID renaming stay GREEN.
#[test]
fn tooth_ordered_unit_equivalence_catches_byte_flips_swaps_duplication_and_crossed_ids() -> Result<()> {
    let mut base = rich_conversation(Harness::Claude);
    // For IR-level diff against Claude, use a source harness whose Notice is Special/InjectedContext
    // or test across Codex / OpenCode / Claude where all conversational + system turns survive.
    base.messages[0].kind = MessageKind::InjectedContext;
    base.harness = Harness::Codex;

    // Baseline self-equivalence is 100% clean.
    let clean = diff_fidelity(Harness::Codex, ThinkingMode::Native, &base, &base);
    assert!(!clean.has_unexpected(), "baseline must be clean: {:?}", clean.deltas);

    // 1. Canonical JSON invariance: reordered keys + `-0.0` -> `0` + `42` -> `42.0` stays GREEN.
    {
        let mut reordered = base.clone();
        reordered.messages[2].content[2] = Block::ToolUse {
            id: "renamed_0".into(),
            name: "Read".into(),
            input: json!({
                "zeta": [true, 0],
                "alpha": { "path": "/src/alpha.rs", "count": 42.0 }
            })
            .into(),
            namespace: None,
            extensions: Default::default(),
        };
        reordered.messages[2].content[3] = Block::ToolUse {
            id: "renamed_1".into(),
            name: "Read".into(),
            input: json!({
                "zeta": [false, 1.0],
                "alpha": { "path": "/src/beta.rs", "count": 7 }
            })
            .into(),
            namespace: None,
            extensions: Default::default(),
        };
        if let Block::ToolResult { tool_use_id, .. } = &mut reordered.messages[3].content[0] {
            *tool_use_id = "renamed_0".into();
        }
        if let Block::ToolResult { tool_use_id, .. } = &mut reordered.messages[3].content[1] {
            *tool_use_id = "renamed_1".into();
        }
        let rep = diff_fidelity(Harness::Claude, ThinkingMode::Native, &base, &reordered);
        assert!(
            !rep.has_unexpected(),
            "canonical JSON key reorder + injective ρ rename must stay GREEN, got {:?}",
            rep.deltas
        );
    }

    // 2. 1-byte flip in Assistant `Block::Text` -> RED!
    {
        let mut flipped = base.clone();
        flipped.messages[5].content[0] = Block::from_text("Final synthesis completf.".into());
        let rep = diff_fidelity(Harness::Claude, ThinkingMode::Native, &base, &flipped);
        assert!(rep.has_unexpected(), "1-byte Assistant text flip must fail --strict");
        assert!(
            rep.deltas
                .iter()
                .any(|d| !d.expected && d.what.contains("turn[5].block[0] text")),
            "expected turn[5].block[0] text locator, got {:?}",
            rep.deltas
        );
    }

    // 3. 1-byte flip in `Role::System` `InjectedContext` text -> RED!
    {
        let mut flipped_sys = base.clone();
        flipped_sys.messages[0].content[0] = Block::from_text("system guardrail alphb".into());
        let rep = diff_fidelity(Harness::Claude, ThinkingMode::Native, &base, &flipped_sys);
        assert!(rep.has_unexpected(), "1-byte System text flip must fail --strict");
        assert!(
            rep.deltas
                .iter()
                .any(|d| !d.expected && d.what.contains("turn[0].block[0] text")),
            "expected turn[0].block[0] text locator, got {:?}",
            rep.deltas
        );
    }

    // 4. 1-byte flip in `Block::ToolUse.input` nested JSON value -> RED!
    {
        let mut flipped_input = base.clone();
        flipped_input.messages[2].content[2] = Block::ToolUse {
            id: "call_alpha".into(),
            name: "Read".into(),
            input: json!({
                "alpha": { "count": 42, "path": "/src/alpha.rt" },
                "zeta": [true, -0.0]
            })
            .into(),
            namespace: None,
            extensions: Default::default(),
        };
        let rep = diff_fidelity(Harness::Claude, ThinkingMode::Native, &base, &flipped_input);
        assert!(rep.has_unexpected(), "1-byte tool_use.input flip must fail --strict");
        assert!(
            rep.deltas
                .iter()
                .any(|d| !d.expected && d.what.contains("turn[2].block[2] tool_use.input")),
            "expected turn[2].block[2] tool_use.input locator, got {:?}",
            rep.deltas
        );
    }

    // 5. Swapping two assistant replies (preserving all role/kind/block counts!) -> RED!
    {
        let mut swapped = base.clone();
        swapped.messages[2].content[1] = Block::from_text("Final synthesis complete.".into());
        swapped.messages[5].content[0] = Block::from_text("Running parallel reads.".into());
        let rep = diff_fidelity(Harness::Claude, ThinkingMode::Native, &base, &swapped);
        assert!(rep.has_unexpected(), "swapped assistant replies must fail --strict");
        assert!(
            rep.deltas
                .iter()
                .any(|d| !d.expected && d.what.contains("turn[2].block[1] text")),
            "expected turn[2].block[1] text mismatch, got {:?}",
            rep.deltas
        );
    }

    // 6. Duplicating a turn (`after > before`) -> RED!
    {
        let mut dup = base.clone();
        let extra = dup.messages[5].clone();
        dup.messages.push(extra);
        let rep = diff_fidelity(Harness::Claude, ThinkingMode::Native, &base, &dup);
        assert!(rep.has_unexpected(), "duplicated assistant turn must fail --strict");
        assert!(
            rep.deltas
                .iter()
                .any(|d| !d.expected && d.what.contains("unexpected extra")),
            "expected 'unexpected extra' diagnostic, got {:?}",
            rep.deltas
        );
    }

    // 7. Crossing two parallel `tool_use_id` ↔ `tool_result` links -> RED!
    {
        let mut crossed = base.clone();
        if let Block::ToolResult { tool_use_id, .. } = &mut crossed.messages[3].content[0] {
            *tool_use_id = "call_beta".into();
        }
        if let Block::ToolResult { tool_use_id, .. } = &mut crossed.messages[3].content[1] {
            *tool_use_id = "call_alpha".into();
        }
        let rep = diff_fidelity(Harness::Claude, ThinkingMode::Native, &base, &crossed);
        assert!(rep.has_unexpected(), "crossed parallel tool_use_ids must fail --strict");
        assert!(
            rep.deltas
                .iter()
                .any(|d| !d.expected && d.what.contains("tool_result.tool_use_id (ρ)")),
            "expected tool_result.tool_use_id (ρ) diagnostic, got {:?}",
            rep.deltas
        );
    }

    // 8. Non-injective ID collapse (`call_alpha` and `call_beta` both mapped to `"same"`) -> RED!
    {
        let mut collapsed = base.clone();
        if let Block::ToolUse { id, .. } = &mut collapsed.messages[2].content[2] {
            *id = "same".into();
        }
        if let Block::ToolUse { id, .. } = &mut collapsed.messages[2].content[3] {
            *id = "same".into();
        }
        if let Block::ToolResult { tool_use_id, .. } = &mut collapsed.messages[3].content[0] {
            *tool_use_id = "same".into();
        }
        if let Block::ToolResult { tool_use_id, .. } = &mut collapsed.messages[3].content[1] {
            *tool_use_id = "same".into();
        }
        let rep = diff_fidelity(Harness::Claude, ThinkingMode::Native, &base, &collapsed);
        assert!(
            rep.has_unexpected(),
            "collapsed non-injective tool IDs must fail --strict"
        );
        assert!(
            rep.deltas
                .iter()
                .any(|d| !d.expected && d.what.contains("tool_use.id (ρ)")),
            "expected tool_use.id (ρ) diagnostic, got {:?}",
            rep.deltas
        );
    }

    // 9. Surviving `Block::Image` and `Block::File` 1-byte flip -> RED!
    {
        let mut with_media = base.clone();
        with_media.messages[1].content.push(Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("data:image/png;base64,AAAA".into()),
            extensions: Default::default(),
        });
        with_media.messages[1].content.push(Block::File {
            mime: Some("text/plain".into()),
            path: Some("notes/spec.md".into()),
            source: Some("file:///notes/spec.md".into()),
            extensions: Default::default(),
        });
        let clean_media = diff_fidelity(Harness::Claude, ThinkingMode::Native, &with_media, &with_media);
        assert!(!clean_media.has_unexpected(), "{:?}", clean_media.deltas);

        let mut bad_img = with_media.clone();
        bad_img.messages[1].content[1] = Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("data:image/png;base64,AAAB".into()),
            extensions: Default::default(),
        };
        let rep_img = diff_fidelity(Harness::Claude, ThinkingMode::Native, &with_media, &bad_img);
        assert!(
            rep_img.deltas.iter().any(|d| !d.expected && d.what.contains("image")),
            "1-byte image flip must fail --strict, got {:?}",
            rep_img.deltas
        );

        let mut bad_file = with_media.clone();
        bad_file.messages[1].content[2] = Block::File {
            mime: Some("text/plain".into()),
            path: Some("notes/spec.md".into()),
            source: Some("file:///notes/sped.md".into()),
            extensions: Default::default(),
        };
        let rep_file = diff_fidelity(Harness::Claude, ThinkingMode::Native, &with_media, &bad_file);
        assert!(
            rep_file.deltas.iter().any(|d| !d.expected && d.what.contains("file")),
            "1-byte file flip must fail --strict, got {:?}",
            rep_file.deltas
        );
    }
    Ok(())
}

/// Snapshot every regular file (relative path -> exact bytes) under `dir`.
fn snapshot_tree(dir: &Path) -> Result<std::collections::BTreeMap<PathBuf, Vec<u8>>> {
    let mut map = std::collections::BTreeMap::new();
    if !dir.exists() {
        return Ok(map);
    }
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(dir)?.to_path_buf();
            map.insert(rel, fs::read(entry.path())?);
        }
    }
    Ok(map)
}

/// Construct a session that triggers an unexpected `--strict` loss on `target` (an image without
/// embedded data or a foreign `Role::System` text turn that `Claude` drops, or a tampered staged
/// file via `emitter_for`).
fn strict_failing_session_for_file_target(target: Harness) -> Session {
    let mut s = Session::new("lossy-src", Harness::OpenCode);
    s.cwd = Some(PathBuf::from("/work/strict-tooth"));
    s.created_at = Some("2026-10-01T12:00:00Z".parse().unwrap());
    s.updated_at = Some("2026-10-01T12:01:00Z".parse().unwrap());
    if target == Harness::Claude {
        // Foreign plain `Role::System` Notice text is unwritten on Claude, causing unexpected `block:text` loss.
        let mut sys = Message::new(Role::System);
        sys.content.push(Block::from_text("unwritten foreign notice".into()));
        s.messages.push(sys);
    } else {
        // Every other file target with `Field::Image` (`Gemini`, `Qwen`, `Continue`) drops or
        // degrades a `None`-ref image (`block:image: 1 -> 0` unexpected!), and for `Codex`/`Grok`/
        // `OpenClaw`/`Cline`/`Roo`/`LmStudio` we drop `Role::Tool` non-result text (`block:text` loss!).
        let mut user = Message::new(Role::User);
        user.content.push(Block::from_text("hello".into()));
        s.messages.push(user);
        let mut tool = Message::new(Role::Tool);
        tool.content = vec![
            Block::from_text("dropped non-result text on tool turn".into()),
            Block::ToolResult {
                tool_use_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
                tool_name: Some("Read".into()),
                status: None,
                details: None,
                extensions: Default::default(),
            },
        ];
        s.messages.push(tool);
    }
    s
}

/// `#tooth` Gate 2a: Across ALL file/directory targets (`Claude`, `Codex`, `Grok`, `OpenClaw`,
/// `Gemini`, `LmStudio`, `Cline`, `Roo`, `Continue`, `Qwen`), when `--strict` fails:
/// 1. A non-existent `out_dir` stays non-existent (and zero `.cv-stage-*` temporaries remain in its parent!);
/// 2. With `force: true` targeting an existing session ID, every pre-existing file in `out_dir`
///    remains 100% byte-for-byte identical!
#[test]
fn tooth_file_targets_stage_before_publish_leaving_zero_files_and_preserving_force_targets() -> Result<()> {
    let world = crate::test_world("tooth-file-stage");
    let file_targets = [
        Harness::Claude,
        Harness::Codex,
        Harness::Grok,
        Harness::OpenClaw,
        Harness::Gemini,
        Harness::LmStudio,
        Harness::Cline,
        Harness::Roo,
        Harness::Continue,
        Harness::Qwen,
    ];

    let mut clean_seed = Session::new("seed", Harness::Claude);
    clean_seed.cwd = Some(PathBuf::from("/work/strict-tooth"));
    clean_seed.title = Some("seed title".into());
    clean_seed.model = Some("claude-sonnet-4-6".into());
    let ts: chrono::DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
    clean_seed.created_at = Some(ts);
    clean_seed.updated_at = Some(ts);
    let mut u = Message::new(Role::User);
    u.timestamp = Some(ts);
    u.content.push(Block::from_text("original intact prompt".into()));
    let mut a = Message::new(Role::Assistant);
    a.timestamp = Some(ts);
    a.model = Some("claude-sonnet-4-6".into());
    a.content.push(Block::from_text("original intact reply".into()));
    clean_seed.messages = vec![u, a];

    for target in file_targets {
        let parent = world.base.join(format!("case-{target}"));
        fs::create_dir_all(&parent)?;
        let fresh_out = parent.join("fresh-out");

        let mut lossy = strict_failing_session_for_file_target(target);
        if target == Harness::LmStudio {
            // LM Studio has no tool structures: trigger unexpected `per-message model` loss via empty session model + unrepresented message.
            lossy = clean_seed.clone();
            lossy.model = None;
            lossy.messages[0].model = Some("user-model-unheld".into());
        }

        // Case A: Non-existent `fresh_out` stays completely absent when `--strict` fails, and zero
        // `.cv-stage-*` sibling workspaces remain in `parent`.
        let strict_opts = EmitOptions {
            new_id: Some("pinned-session".into()),
            strict: true,
            ..Default::default()
        };
        let Err(err) = emit_verified(&lossy, target, &fresh_out, &strict_opts) else {
            panic!("{target}: lossy session must fail --strict");
        };
        assert!(
            format!("{err:#}").contains("--strict"),
            "{target}: error must cite --strict, got {err:#}"
        );
        assert!(
            !fresh_out.exists(),
            "{target}: non-existent out_dir must NOT be created when --strict fails"
        );
        assert_eq!(
            fs::read_dir(&parent)?.count(),
            0,
            "{target}: zero .cv-stage-* directories may remain in parent after rollback"
        );

        // Case B: Seed a clean session at `"pinned-session"`, snapshot every file's bytes, then
        // attempt `force: true, strict: true` overwrite that fails verification (both via lossy
        // session AND via direct 1-byte stage mutation!).
        let live_out = parent.join("live-out");
        let seed_opts = EmitOptions {
            new_id: Some("pinned-session".into()),
            force: false,
            strict: false,
            ..Default::default()
        };
        emit(&clean_seed, target, &live_out, &seed_opts)?;
        let before_tree = snapshot_tree(&live_out)?;
        assert!(!before_tree.is_empty(), "{target}: seed must have written files");

        let force_strict = EmitOptions {
            new_id: Some("pinned-session".into()),
            force: true,
            strict: true,
            ..Default::default()
        };
        assert!(
            emit_verified(&lossy, target, &live_out, &force_strict).is_err(),
            "{target}: forced strict failure must error"
        );
        assert_eq!(
            snapshot_tree(&live_out)?,
            before_tree,
            "{target}: pre-existing --force session files must remain 100% byte-for-byte intact!"
        );

        // Also test direct 1-byte stage corruption on `clean_seed` via `emitter_for`: flip 1 byte
        // of `"original intact reply"` -> `"original intact replz"` inside the staged file right
        // before running the real re-parser + unit verifier!
        let mut alt_seed = clean_seed.clone();
        alt_seed.messages[1].content[0] = Block::from_text("updated reply that will be corrupted in stage".into());
        let emitter = super::registered_emitter(target)?;
        let before_proj = source_projected_with_opts(target, &alt_seed, &force_strict)?;
        let mut before_slot = Some(before_proj);
        let stage_tamper_res = emitter(
            &alt_seed,
            &live_out,
            &force_strict,
            Some(&mut |staged_src| {
                if let StagedSource::Files { result } = staged_src {
                    // Corrupt 1 byte in the staged transcript file before re-parsing!
                    let transcript = if result.path.is_dir() {
                        ["chat_history.jsonl", "api_conversation_history.json"]
                            .into_iter()
                            .map(|n| result.path.join(n))
                            .find(|p| p.is_file())
                            .unwrap()
                    } else {
                        result.path.clone()
                    };
                    let text = fs::read_to_string(&transcript)?;
                    let corrupted = text.replace(
                        "updated reply that will be corrupted in stage",
                        "updated reply that will be corrupted in stagX",
                    );
                    assert_ne!(text, corrupted, "{target}: staged file must contain reply text");
                    fs::write(&transcript, corrupted)?;
                }
                let before = before_slot.take().unwrap();
                let (reparsed, after) = reparse_staged_projected(
                    target,
                    force_strict.thinking,
                    staged_src,
                    &force_strict.parsed_at.demand(),
                )?;
                let report =
                    super::fidelity::diff_projected(target, force_strict.thinking, &alt_seed, &reparsed, before, after);
                anyhow::ensure!(!report.has_unexpected(), "--strict caught 1-byte stage tamper");
                Ok(())
            }),
        );
        assert!(
            stage_tamper_res.is_err(),
            "{target}: 1-byte tamper in staged file must fail strict verifier!"
        );
        assert_eq!(
            snapshot_tree(&live_out)?,
            before_tree,
            "{target}: 1-byte tamper abort must leave pre-existing --force session byte-identical!"
        );
    }
    Ok(())
}

/// `#tooth` Gate 2b (Claude `--skills` pre-publish proof): `SkillsPolicy` rewrites the staged
/// bytes *before* `verify_claude_resumable` and *before* fidelity read-back, so the verified
/// BLAKE3 digest equals the published file's BLAKE3 digest bit-for-bit, `--skills drop --strict`
/// passes cleanly, and a strict failure after `--skills drop` leaves an existing `--force` target
/// with its original `invoked_skills` record untouched!
#[test]
fn tooth_claude_skills_policy_rewrites_inside_stage_before_verification_and_publish() -> Result<()> {
    let world = crate::test_world("tooth-claude-skills-stage");
    let config = world.base.join("claude-cfg");
    let projects = config.join("projects");
    let src_proj = projects.join("-work-proj");
    fs::create_dir_all(&src_proj)?;
    let src_file = src_proj.join("src-sess.jsonl");
    let rows = [
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"src-sess","cwd":"/work/proj",
               "timestamp":"2026-10-01T12:00:00Z","message":{"role":"user","content":"use skill"}}),
        json!({"type":"attachment","uuid":"att1","parentUuid":"u1","sessionId":"src-sess","cwd":"/work/proj",
               "timestamp":"2026-10-01T12:00:01Z","isSidechain":false,
               "attachment":{"type":"invoked_skills","skills":[
                   {"name":"gone","path":"userSettings:gone","content":"Base directory for this skill: /x\n\nOld skill."}
               ]},
               "rendered":[{"content":"<system-reminder>Old skill.</system-reminder>"}],
               "renderedRole":"system"}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"att1","sessionId":"src-sess","cwd":"/work/proj",
               "timestamp":"2026-10-01T12:00:02Z","message":{"role":"assistant","model":"claude-sonnet-4-6",
               "content":[{"type":"text","text":"Skill applied."}]}}),
    ];
    let raw: String = rows.iter().map(|r| format!("{r}\n")).collect();
    fs::write(&src_file, &raw)?;

    let adapter = crate::harness::claude::Claude::with_config_dir(&config);
    let r = crate::ir::SessionRef::new("src-sess", Harness::Claude, &src_file);
    let session = parse_for_emit(&adapter, &r, Harness::Claude)?;

    // 1. Capture the BLAKE3 digest of the staged file *inside* `verify` and prove it equals the
    //    published file's BLAKE3 digest bit-for-bit!
    let out_dir = projects.clone();
    let opts = EmitOptions {
        new_id: Some("ported-clean".into()),
        strict: true,
        thinking: ThinkingMode::Native,
        parsed_at: ParsedAt::Complete,
        skills_policy: SkillsPolicy::Drop,
        skills_config: Some(config.clone()),
        ..Default::default()
    };
    let mut staged_blake3: Option<blake3::Hash> = None;
    let res = crate::harness::claude::emit_staged(
        &session,
        &out_dir,
        &opts,
        Some(&mut |staged_src| {
            if let StagedSource::Files { result } = staged_src {
                staged_blake3 = Some(blake3::hash(&fs::read(&result.path)?));
            }
            Ok(())
        }),
    )?;
    let published_bytes = fs::read(&res.path)?;
    assert_eq!(
        staged_blake3,
        Some(blake3::hash(&published_bytes)),
        "published bytes MUST equal the exact staged post-skills bytes verified before publish!"
    );
    assert_eq!(res.skills.records, 1);
    assert_eq!(res.skills.dropped, 1);

    // 2. Full `emit_verified` with `--skills drop --strict` succeeds cleanly!
    let (verified_res, warnings) = emit_verified(
        &session,
        Harness::Claude,
        &out_dir,
        &EmitOptions {
            new_id: Some("ported-strict".into()),
            ..opts.clone()
        },
    )?;
    assert_eq!(verified_res.skills.dropped, 1);
    assert!(
        warnings.iter().any(|w| w.contains("system:invoked_skills")),
        "expected transparent invoked_skills policy line, got {warnings:?}"
    );

    // 3. Seed `"keep-skills"` with `SkillsPolicy::Keep`, then attempt `force: true, strict: true,
    //    skills_policy: Drop` with an extra unwritten foreign System notice so `--strict` fails:
    //    `"keep-skills"` MUST retain its original un-dropped `invoked_skills` + `rendered` bytes!
    let seed_res = emit(
        &session,
        Harness::Claude,
        &out_dir,
        &EmitOptions {
            new_id: Some("keep-skills".into()),
            parsed_at: ParsedAt::Complete,
            skills_policy: SkillsPolicy::Keep,
            ..Default::default()
        },
    )?;
    let seed_bytes = fs::read(&seed_res.path)?;
    assert!(
        String::from_utf8_lossy(&seed_bytes).contains("Old skill."),
        "seed keeps skill content"
    );

    let mut failing = session.clone();
    let mut bad_sys = Message::new(Role::System);
    bad_sys.content.push(Block::from_text("unwritten extra notice".into()));
    failing.messages.push(bad_sys);
    assert!(
        emit_verified(
            &failing,
            Harness::Claude,
            &out_dir,
            &EmitOptions {
                new_id: Some("keep-skills".into()),
                force: true,
                strict: true,
                parsed_at: ParsedAt::Complete,
                skills_policy: SkillsPolicy::Drop,
                skills_config: Some(config),
                ..Default::default()
            }
        )
        .is_err(),
        "strict failure must abort before publishing skill-rewritten file"
    );
    assert_eq!(
        fs::read(&seed_res.path)?,
        seed_bytes,
        "existing --force Claude transcript must keep its original un-rewritten bytes on strict failure!"
    );
    Ok(())
}

/// `#tooth` Gate 3: Same-Transaction SQLite Verification & Rollback for `OpenCode` (v1 & v2),
/// `Hermes` (including FTS5 `messages_fts` triggers & autoincrement `rowid`s), and `OpenClaw` SQLite.
#[cfg(feature = "sqlite")]
#[test]
fn tooth_sqlite_targets_verify_in_same_begin_immediate_tx_and_rollback_force_intact() -> Result<()> {
    let world = crate::test_world("tooth-sqlite-tx");
    let ts: chrono::DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();

    let make_clean = |reply: &str| {
        let mut s = Session::new("clean", Harness::Claude);
        s.cwd = Some(PathBuf::from("/work/sqlite-tooth"));
        s.title = Some(format!("title for {reply}"));
        s.model = Some("claude-sonnet-4-6".into());
        s.created_at = Some(ts);
        s.updated_at = Some(ts);
        let mut u = Message::new(Role::User);
        u.timestamp = Some(ts);
        u.content.push(Block::from_text(format!("prompt for {reply}").into()));
        let mut a = Message::new(Role::Assistant);
        a.timestamp = Some(ts);
        a.model = Some("claude-sonnet-4-6".into());
        a.content.push(Block::from_text(reply.into()));
        s.messages = vec![u, a];
        s
    };

    // Session with non-result `Block::Text` inside `Role::Tool`, which OpenCode/Hermes/OpenClaw
    // cannot emit (`block:text` unexpected loss -> `--strict` RED!).
    let mut lossy = make_clean("lossy attempt");
    let mut bad_tool = Message::new(Role::Tool);
    bad_tool.timestamp = Some(ts);
    bad_tool.content = vec![
        Block::from_text("unemitted text on tool turn".into()),
        Block::ToolResult {
            tool_use_id: "orphan_1".into(),
            content: "tool out".into(),
            is_error: false,
            tool_name: Some("Bash".into()),
            status: None,
            details: None,
            extensions: Default::default(),
        },
    ];
    lossy.messages.push(bad_tool);

    // ── A. Fresh non-existent SQLite target directories leave ZERO `.db` files on `--strict` failure ──
    for (target, id) in [
        (Harness::OpenCode, "ses_fresh_fail"),
        (Harness::Hermes, "hermes-fresh-fail"),
    ] {
        let fresh_dir = world.base.join(format!("fresh-{target}"));
        let Err(err) = emit_verified(
            &lossy,
            target,
            &fresh_dir,
            &EmitOptions {
                new_id: Some(id.into()),
                strict: true,
                ..Default::default()
            },
        ) else {
            panic!("{target}: fresh SQLite strict failure expected");
        };
        assert!(format!("{err:#}").contains("--strict"), "{err:#}");
        assert!(
            !fresh_dir.exists(),
            "{target}: failed fresh SQLite emit must remove empty .db and newly created directories!"
        );
    }

    // ── B. OpenCode v2 live database: project resolution, same-tx tamper & `--force` rollback ──
    {
        let oc_v2_dir = world.base.join("opencode-v2");
        emit(
            &make_clean("v2 bystander alpha"),
            Harness::OpenCode,
            &oc_v2_dir,
            &EmitOptions {
                new_id: Some("ses_keep_v2".into()),
                ..Default::default()
            },
        )?;
        let target_res = emit(
            &make_clean("v2 original target"),
            Harness::OpenCode,
            &oc_v2_dir,
            &EmitOptions {
                new_id: Some("ses_target_v2".into()),
                ..Default::default()
            },
        )?;
        let dump_v2 = |db: &Path| -> Result<Vec<(String, String, i64, String)>> {
            let conn = rusqlite::Connection::open(db)?;
            let mut stmt =
                conn.prepare("SELECT session_id, id, seq, data FROM session_message ORDER BY session_id, seq")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        };
        let snapshot_before = dump_v2(&target_res.path)?;
        assert_eq!(snapshot_before.len(), 4, "2 sessions * 2 messages each");

        // 1. `--force` + `--strict` failure rolls back DELETEs + INSERTs, preserving `ses_target_v2`!
        assert!(emit_verified(
            &lossy,
            Harness::OpenCode,
            &oc_v2_dir,
            &EmitOptions {
                new_id: Some("ses_target_v2".into()),
                force: true,
                strict: true,
                ..Default::default()
            },
        )
        .is_err());
        assert_eq!(
            dump_v2(&target_res.path)?,
            snapshot_before,
            "OpenCode v2 --force rollback must preserve exact message IDs, seqs, and JSON data!"
        );

        // 2. Direct uncommitted row corruption inside `&tx` before `verify` runs: corrupt 1 byte in
        //    `session_message.data` on `conn` (`&tx`), run same-tx `reparse_staged_projected`, and
        //    verify `tx` rolls back so `ses_target_v2` keeps its original rows!
        let replacement = make_clean("v2 replacement to tamper");
        let force_opts = EmitOptions {
            new_id: Some("ses_target_v2".into()),
            force: true,
            strict: true,
            ..Default::default()
        };
        let before_proj = source_projected_with_opts(Harness::OpenCode, &replacement, &force_opts)?;
        let mut before_slot = Some(before_proj);
        let tamper = crate::harness::opencode::emit_staged(
            &replacement,
            &oc_v2_dir,
            &force_opts,
            Some(&mut |staged_src| {
                if let StagedSource::Sqlite { conn, result } = staged_src {
                    conn.execute(
                        "UPDATE session_message SET data = replace(data, 'v2 replacement to tamper', 'v2 replacement to tampeZ') WHERE session_id = ?1",
                        [&result.new_id],
                    )?;
                }
                let before = before_slot.take().unwrap();
                let (reparsed, after) = reparse_staged_projected(
                    Harness::OpenCode,
                    force_opts.thinking,
                    staged_src,
                    &force_opts.parsed_at.demand(),
                )?;
                let report = super::fidelity::diff_projected(
                    Harness::OpenCode,
                    force_opts.thinking,
                    &replacement,
                    &reparsed,
                    before,
                    after,
                );
                anyhow::ensure!(!report.has_unexpected(), "caught same-tx OpenCode v2 tamper");
                Ok(())
            }),
        );
        assert!(tamper.is_err(), "1-byte SQLite in-tx tamper must fail verifier");
        assert_eq!(
            dump_v2(&target_res.path)?,
            snapshot_before,
            "OpenCode v2 in-tx tamper rollback must leave pre-existing session 100% intact!"
        );
    }

    // ── C. OpenCode v1 live database: `--force` rollback preserves `session`/`message`/`part` ──
    {
        let oc_v1_dir = world.base.join("opencode-v1");
        fs::create_dir_all(&oc_v1_dir)?;
        let v1_db = oc_v1_dir.join("opencode.db");
        {
            let conn = rusqlite::Connection::open(&v1_db)?;
            conn.execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, model TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
                 CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
            )?;
        }
        emit(
            &make_clean("v1 original target"),
            Harness::OpenCode,
            &oc_v1_dir,
            &EmitOptions {
                new_id: Some("ses_target_v1".into()),
                ..Default::default()
            },
        )?;
        let dump_v1 = |db: &Path| -> Result<Vec<(String, String)>> {
            let conn = rusqlite::Connection::open(db)?;
            let mut stmt = conn.prepare("SELECT id, data FROM part ORDER BY id")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        };
        let parts_before = dump_v1(&v1_db)?;
        assert_eq!(parts_before.len(), 2);
        assert!(emit_verified(
            &lossy,
            Harness::OpenCode,
            &oc_v1_dir,
            &EmitOptions {
                new_id: Some("ses_target_v1".into()),
                force: true,
                strict: true,
                ..Default::default()
            },
        )
        .is_err());
        assert_eq!(
            dump_v1(&v1_db)?,
            parts_before,
            "OpenCode v1 --force rollback must preserve exact part IDs and data!"
        );
    }

    // ── D. Hermes live `state.db`: autoincrement `rowid`s & FTS5 `messages_fts` triggers roll back intact! ──
    {
        let hermes_dir = world.base.join("hermes-live");
        emit(
            &make_clean("hermes bystander"),
            Harness::Hermes,
            &hermes_dir,
            &EmitOptions {
                new_id: Some("hermes-keep".into()),
                ..Default::default()
            },
        )?;
        let h_res = emit(
            &make_clean("hermes target originalsearchabletoken"),
            Harness::Hermes,
            &hermes_dir,
            &EmitOptions {
                new_id: Some("hermes-target".into()),
                ..Default::default()
            },
        )?;
        type HermesMsgRow = (i64, String, Option<String>);
        type HermesFtsRow = (i64, String);
        let dump_hermes = |db: &Path| -> Result<(Vec<HermesMsgRow>, Vec<HermesFtsRow>)> {
            let conn = rusqlite::Connection::open(db)?;
            let msgs = conn
                .prepare("SELECT id, session_id, content FROM messages ORDER BY id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let fts = conn
                .prepare("SELECT rowid, content FROM messages_fts ORDER BY rowid")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((msgs, fts))
        };
        let (msgs_before, fts_before) = dump_hermes(&h_res.path)?;
        assert_eq!(msgs_before.len(), 4);
        assert_eq!(fts_before.len(), 4);
        assert!(fts_before.iter().any(|(_, c)| c.contains("originalsearchabletoken")));

        assert!(emit_verified(
            &lossy,
            Harness::Hermes,
            &hermes_dir,
            &EmitOptions {
                new_id: Some("hermes-target".into()),
                force: true,
                strict: true,
                ..Default::default()
            },
        )
        .is_err());
        let (msgs_after, fts_after) = dump_hermes(&h_res.path)?;
        assert_eq!(
            msgs_after, msgs_before,
            "Hermes --force rollback must preserve exact messages rows & rowids!"
        );
        assert_eq!(
            fts_after, fts_before,
            "Hermes --force rollback must preserve exact FTS5 messages_fts index entries!"
        );
    }

    Ok(())
}
