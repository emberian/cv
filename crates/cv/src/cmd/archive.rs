//! `cv watch` / `cv archive` — durable session custody over multi-record Zstandard-Seekable
//! `packs/` and `StateStore` (`state.db`).
//!
//! Layout under `$CLUSTERVISION_HOME` (default `~/.clustervision`):
//! ```text
//!   packs/<pack_id>.pack.zst       sealed multi-record seekable zstd frames + XXH64 seek table
//!   packs/<pack_id>.pack.idx.json  companion FrameIndex + RecordIndex + CodecCheckpointIndex
//!   state.db                       transactional capture_inbox -> pack_manifests custody ledger
//! ```
//!
//! Each sync pass or watch poll batches every new/changed session into **one** sealed `.pack.zst`
//! archive (instead of creating one inode per session), verifies publish-before-reference by
//! sealing `.pack.zst` + `.pack.idx.json` on disk *before* committing the `pack_manifests` range
//! that drains `capture_inbox`, and deduplicates unchanged sessions by
//! `(origin_producer, harness, session_id)` + source stat + canonical IR BLAKE3 digest.
//!
//! `cv archive ls` and `cv archive path` open `state.db` strictly read-only (`list_pack_manifests`)
//! and succeed under `--frozen` without mutating disk even on a cold/absent `$CLUSTERVISION_HOME`.

use crate::util::{fmt_local, parse_harness, poll_interval, short_id};
use anyhow::{bail, Context, Result};
use chrono::Utc;
pub use cv_core::custody::Archive;
use cv_core::custody::WatchBaselines;
use cv_core::state::StateStore;
use cv_core::{Harness, SessionRef};
use std::path::PathBuf;

fn truncate_str(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() && max_chars > 1 {
        let mut trimmed: String = s.chars().take(max_chars - 1).collect();
        trimmed.push('…');
        trimmed
    } else {
        head
    }
}

fn discover_filtered(harness: Option<Harness>, cwd_sub: Option<&str>) -> Vec<SessionRef> {
    // Custody-backed refs are this archive's own packs (or hub mirrors): never re-archive them.
    cv_core::discover_all()
        .into_iter()
        .filter(|r| !cv_core::custody::is_pack_ref(r))
        .filter(|r| harness.is_none_or(|h| r.harness == h))
        .filter(|r| match cwd_sub {
            None => true,
            Some(sub) => r
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().contains(sub))
                .unwrap_or(false),
        })
        .collect()
}

fn resolve_shipper(
    archive: &Archive,
    target: &str,
    cli_token: Option<&str>,
) -> Result<(String, cv_core::state::hub::Shipper)> {
    let target = target.trim();
    if target.is_empty() {
        bail!("--to target must not be empty (pass an `https://host` or `http://host:port` hub URL or a `[replication.<name>]` profile)");
    }
    let cfg = cv_core::config::load();
    let (target_name, hub_url, profile) = if target.starts_with("http://") || target.starts_with("https://") {
        let prof = cv_core::state::hub::resolve_replication_profile(&cfg, target);
        (target.to_string(), target.to_string(), prof)
    } else {
        if !cfg.replication.contains_key(target) {
            bail!(
                "unknown replication target `{target}`: pass an `https://host` or `http://host:port` URL or configure `[replication.{target}]` with `url = \"https://...\"` in config.toml"
            );
        }
        let prof = cv_core::state::hub::resolve_replication_profile(&cfg, target);
        let Some(url) = prof.url.clone().filter(|u| !u.trim().is_empty()) else {
            bail!("replication profile `[replication.{target}]` has no `url = \"https://...\"` in config.toml");
        };
        (target.to_string(), url, prof)
    };

    let bearer_token = cli_token
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            cv_core::env::var("CV_HUB_PRODUCER_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| profile.token.clone().filter(|s| !s.trim().is_empty()));

    let store = archive.state_store();
    let producer_id = profile
        .producer_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            cv_core::env::var("CV_PRODUCER_ID")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .map(Ok)
        .unwrap_or_else(|| store.ensure_ship_producer_id())
        .context("resolving replication producer_id")?;

    let mut ship_cfg = cv_core::state::hub::ShipConfig::new(target_name.clone(), hub_url, producer_id)
        .with_host_label(cv_core::state::default_host_label())
        .with_profile(profile);
    if let Some(tok) = bearer_token {
        ship_cfg = ship_cfg.with_token(tok);
    }
    let shipper = cv_core::state::hub::Shipper::new(ship_cfg).context("initializing hub replication shipper")?;
    Ok((target_name, shipper))
}

fn format_ship_report(target_name: &str, report: &cv_core::state::hub::ShipReport) -> String {
    if report.events_shipped == 0 {
        return format!("  ↳ replicated to {target_name}: already up to date");
    }
    let snap_note = report
        .hub_snapshot_id
        .as_deref()
        .map(|id| format!(" · snapshot {}", truncate_str(id, 20)))
        .unwrap_or_default();
    format!(
        "  ↳ replicated to {target_name}: {} event(s) ({} session(s), {} filtered, {} redacted){snap_note}",
        report.events_shipped, report.sessions_admitted, report.sessions_filtered_out, report.redactions_applied
    )
}

#[derive(Default)]
pub struct SyncArgs {
    pub to: Option<String>,
    pub token: Option<String>,
    pub spool: Option<PathBuf>,
    pub seal: bool,
    pub ingest_spool: Option<PathBuf>,
    pub pull: Option<String>,
    pub export_delta: Option<String>,
}

fn resolve_local_producer_id(store: &StateStore) -> Result<String> {
    if let Ok(explicit) = cv_core::env::var("CV_PRODUCER_ID") {
        if !explicit.trim().is_empty() {
            return Ok(explicit.trim().to_string());
        }
    }
    if cv_core::env::frozen() {
        if let Some(local) = store.get_producer(store.default_producer())? {
            if let Some(id) = local.metadata["ship_producer_id"].as_str().filter(|s| !s.is_empty()) {
                return Ok(id.to_owned());
            }
        }
        return Ok(format!("host-{}", cv_core::state::default_host_label()));
    }
    store
        .ensure_ship_producer_id()
        .context("resolving local replication producer_id")
}

pub fn cmd_sync(archive: &Archive, args: SyncArgs) -> Result<()> {
    // 1. Read-only `--export-delta <WATERMARK>` mode (runs cleanly under `--frozen`, writing pure
    //    `CVDLT001` binary delta stream to stdout for `cv archive sync --pull <SSH_TARGET>`).
    if let Some(ref watermark) = args.export_delta {
        let store = archive.state_store().for_export();
        let producer_id = resolve_local_producer_id(&store)?;
        let cfg = cv_core::config::load();
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        cv_core::state::hub::export_delta_stream(
            &store,
            &archive.packs_dir(),
            &producer_id,
            &cv_core::state::default_host_label(),
            &cfg.privacy,
            watermark,
            &mut handle,
        )
        .map_err(|e| match e {
            cv_core::state::hub::HubError::InvalidArgument(msg) => anyhow::Error::from(crate::util::UsageError(msg)),
            other => anyhow::Error::from(other),
        })?;
        return Ok(());
    }

    // 2. Hub spool ingest (`cv archive sync --ingest-spool <DIR>`): idempotently import sandbox spool.
    if let Some(ref spool_dir) = args.ingest_spool {
        let hub = cv_core::state::hub::HubStore::at_home(&archive.home);
        let rep = hub.ingest_spool_dir(spool_dir).map_err(|e| match e {
            cv_core::state::hub::HubError::InvalidArgument(msg) => anyhow::Error::from(crate::util::UsageError(msg)),
            other => anyhow::Error::from(other),
        })?;
        cv_search::fts::index_changed_heads(&hub).context("spool committed; updating search and event indexes")?;
        let gen_note = rep
            .hub_generation
            .map(|g| format!(" · generation {g}"))
            .unwrap_or_default();
        println!(
            "✦ ingested {} batch(es) ({} accepted, {} replayed) from {} → {}{gen_note}",
            rep.batches_ingested,
            rep.events_accepted,
            rep.events_replayed,
            spool_dir.display(),
            archive.home.display(),
        );
        return Ok(());
    }

    // 3. Agentless SSH `--frozen` pull (`cv archive sync --pull <SSH_TARGET>`).
    if let Some(ref ssh_target) = args.pull {
        let ssh_target = ssh_target.trim();
        if ssh_target.is_empty()
            || ssh_target.starts_with('-')
            || !ssh_target
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.@:%[]".contains(&b))
        {
            return crate::util::usage("invalid --pull SSH target");
        }
        let hub = cv_core::state::hub::HubStore::at_home(&archive.home);
        let watermark = hub.pull_watermark_for(ssh_target)?;
        let ssh_bin = cv_core::env::var("CV_SSH_BIN").unwrap_or_else(|_| "ssh".into());
        let remote_cv = cv_core::env::var("CV_REMOTE_BIN").unwrap_or_else(|_| "cv".into());
        if remote_cv.is_empty()
            || remote_cv.starts_with('-')
            || !remote_cv
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
        {
            return crate::util::usage("CV_REMOTE_BIN must name a shell-safe executable path");
        }
        let mut child = std::process::Command::new(&ssh_bin)
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=3",
                "--",
                ssh_target,
                &remote_cv,
                "--frozen",
                "archive",
                "sync",
                "--export-delta",
                &watermark,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning `{ssh_bin}` to pull delta from `{ssh_target}`"))?;
        let stdout = child.stdout.take().expect("piped SSH output");
        let result = hub.ingest_delta_stream_checked(std::io::BufReader::new(stdout), Some(ssh_target), || {
            let status = child.wait()?;
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!("ssh exited with {status}")))
            }
        });
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let rep = result.with_context(|| format!("ingesting delta stream pulled from `{ssh_target}`"))?;
        cv_search::fts::index_changed_heads(&hub).context("pull committed; updating search and event indexes")?;
        let gen_note = rep
            .hub_generation
            .map(|g| format!(" · generation {g}"))
            .unwrap_or_default();
        println!(
            "✦ pulled {} new event(s) ({} replayed) from {ssh_target} → {}{gen_note}",
            rep.events_accepted,
            rep.events_replayed,
            archive.home.display(),
        );
        return Ok(());
    }

    // Validate `--to` upfront before spending time syncing locally if the target profile is missing.
    let resolved_shipper = match args.to.as_deref() {
        Some(target) => Some(resolve_shipper(archive, target, args.token.as_deref())?),
        None => None,
    };

    let refs = discover_filtered(None, None);
    let total = refs.len();
    let summary = archive.sync_refs(&refs)?;
    for (who, why) in &summary.failed {
        eprintln!("  ! {who}: {why}");
    }
    let failed = summary.failed.len();
    let pack_note = summary
        .pack_id
        .as_deref()
        .map(|id| format!(" · pack {id}"))
        .unwrap_or_default();
    println!(
        "✦ {} archived, {} skipped (unchanged){} of {total} discovered → {}{pack_note}",
        summary.archived,
        summary.skipped,
        if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        },
        archive.home.display(),
    );

    if let Some((target_name, shipper)) = resolved_shipper {
        let store = archive.state_store();
        let report = shipper
            .sync(&store, &archive.packs_dir())
            .with_context(|| format!("shipping archive custody to `{target_name}`"))?;
        println!("{}", format_ship_report(&target_name, &report));
    }

    if let Some(ref spool_dir) = args.spool {
        let store = archive.state_store();
        let producer_id = resolve_local_producer_id(&store)?;
        let cfg = cv_core::config::load();
        let rep = cv_core::state::hub::export_to_spool(
            &store,
            &archive.packs_dir(),
            spool_dir,
            &producer_id,
            &cv_core::state::default_host_label(),
            &cfg.privacy,
            args.seal,
        )?;
        println!(
            "  ↳ spooled {} event(s), {} pack(s){} → {}",
            rep.events_spooled,
            rep.packs_written,
            if rep.sealed { " [SEALED]" } else { "" },
            spool_dir.display(),
        );
    }

    Ok(())
}

pub fn cmd_watch(
    archive: &Archive,
    interval_secs: f64,
    harness_filter: Option<String>,
    cwd_filter: Option<String>,
    to: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let want = parse_harness(&harness_filter)?;
    let poll = poll_interval(interval_secs)?;
    let resolved_shipper = match to.as_deref() {
        Some(target) => Some(resolve_shipper(archive, target, token.as_deref())?),
        None => None,
    };

    // Announce immediately to stderr BEFORE initial discovery/sync (`docs/INTERFACE-V2.md` §2:
    // daemons announce on stderr within 1s so piped callers know what is running).
    eprintln!(
        "cv watch: watching (interval {}s{}{}{}) -> {}",
        poll.as_secs_f64(),
        want.map(|h| format!(" · harness={h}")).unwrap_or_default(),
        cwd_filter
            .as_deref()
            .map(|c| format!(" · cwd~{c:?}"))
            .unwrap_or_default(),
        resolved_shipper
            .as_ref()
            .map(|(name, _)| format!(" · to={name}"))
            .unwrap_or_default(),
        archive.home.display(),
    );

    let _recorder = super::start_recorder()?;
    // Initial sweep batches all existing sessions into a single sealed pack.
    let initial_refs = discover_filtered(want, cwd_filter.as_deref());
    let init = archive.sync_refs(&initial_refs)?;
    for (who, why) in &init.failed {
        eprintln!("  ! {who}: {why}");
    }
    eprintln!(
        "  initial sync: {} archived, {} already current",
        init.archived, init.skipped
    );

    let mut ship_retry_pending = false;
    if let Some((ref target_name, ref shipper)) = resolved_shipper {
        let store = archive.state_store();
        match shipper.sync(&store, &archive.packs_dir()) {
            Ok(report) => {
                if report.events_shipped > 0 {
                    eprintln!("{}", format_ship_report(target_name, &report));
                }
            }
            Err(e) => {
                ship_retry_pending = true;
                eprintln!("  ! replicate {target_name}: {e:#}");
            }
        }
    }

    let mut seen_baseline = WatchBaselines::after_sync(archive, &initial_refs, &init)?;

    loop {
        std::thread::sleep(poll);
        let refs = discover_filtered(want, cwd_filter.as_deref());
        let batch = seen_baseline.poll(archive, refs)?;
        for (who, why) in &batch.failed {
            eprintln!("  ! {who}: {why}");
        }
        for entry in &batch.archived_entries {
            let now = fmt_local(Utc::now(), "%H:%M:%S");
            let title = entry.title.as_deref().map(|t| truncate_str(t, 44)).unwrap_or_default();
            println!(
                "  [{now}] {:8} {}  ({:>4} msgs)  {title}",
                entry.harness,
                short_id(&entry.id),
                entry.message_count,
            );
        }
        if let Some((ref target_name, ref shipper)) = resolved_shipper {
            let store = archive.state_store();
            let has_inbox = store.inbox_count().unwrap_or(0) > 0;
            if batch.archived > 0 || ship_retry_pending || has_inbox {
                match shipper.sync(&store, &archive.packs_dir()) {
                    Ok(report) => {
                        ship_retry_pending = false;
                        if report.events_shipped > 0 {
                            println!("{}", format_ship_report(target_name, &report));
                        }
                    }
                    Err(e) => {
                        ship_retry_pending = true;
                        eprintln!("  ! replicate {target_name}: {e:#}");
                    }
                }
            }
        }
    }
}

pub fn cmd_ls(archive: &Archive) -> Result<()> {
    let (entries, pack_count) = archive.entries()?;
    if entries.is_empty() {
        println!(
            "archive empty ({}) — run 'cv archive sync' or 'cv watch'",
            archive.home.display()
        );
        return Ok(());
    }
    println!(
        "{:<16} {:<8} {:<10} {:>6}   {:<24} TITLE",
        "ARCHIVED", "HARNESS", "ID", "MSGS", "PACK"
    );
    println!("{}", "─".repeat(96));
    for e in &entries {
        let dt = fmt_local(e.archived_at, "%Y-%m-%d %H:%M");
        let title = e.title.as_deref().map(|t| truncate_str(t, 42)).unwrap_or_default();
        let pack_short = truncate_str(&e.addr.pack_id, 24);
        println!(
            "{:<16} {:<8} {:<10} {:>4} msgs {:<24} {}",
            dt,
            e.harness,
            short_id(&e.id),
            e.message_count,
            pack_short,
            title,
        );
    }
    println!(
        "\n{} session(s) across {} pack(s) in {}",
        entries.len(),
        pack_count,
        archive.packs_dir().display()
    );
    Ok(())
}

pub fn cmd_path(archive: &Archive) -> Result<()> {
    println!("{}", archive.home.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    #[test]
    fn packs_archive_batches_sessions_deduplicates_verifies_blake3_and_reads_frozen() {
        let world = crate::test_world("archive-packs-unit");
        let a_path = world.write_session(
            "sess-alpha",
            &[
                json!({"type":"user","uuid":"u1","sessionId":"sess-alpha","timestamp":"2026-06-01T10:00:00Z",
                       "message":{"role":"user","content":"alpha prompt"}}),
                json!({"type":"assistant","uuid":"a1","sessionId":"sess-alpha","timestamp":"2026-06-01T10:00:05Z",
                       "message":{"role":"assistant","content":"alpha answer"}}),
            ],
        );
        let b_path = world.write_session(
            "sess-beta",
            &[
                json!({"type":"user","uuid":"u2","sessionId":"sess-beta","timestamp":"2026-06-01T11:00:00Z",
                       "message":{"role":"user","content":"beta prompt"}}),
            ],
        );

        let archive = Archive {
            home: world.cv_home.clone(),
        };
        let refs = vec![
            SessionRef::new("sess-alpha", Harness::Claude, &a_path),
            SessionRef::new("sess-beta", Harness::Claude, &b_path),
        ];

        // 1. First sync packs BOTH sessions into ONE sealed `.pack.zst` and drains `capture_inbox`.
        let first = archive.sync_refs(&refs).unwrap();
        assert_eq!(first.archived, 2);
        assert_eq!(first.skipped, 0);
        assert!(first.pack_id.is_some());
        assert!(!world.cv_home.join("archive").exists(), "legacy archive/ dir is gone");
        assert!(
            !world.cv_home.join("catalog.jsonl").exists(),
            "legacy catalog.jsonl is gone"
        );

        let store = archive.state_store();
        assert_eq!(
            store.inbox_count().unwrap(),
            0,
            "capture_inbox drained into pack_manifests"
        );
        let manifests = store.list_pack_manifests().unwrap();
        assert_eq!(manifests.len(), 1, "single sync pass produces exactly ONE pack");
        assert_eq!(manifests[0].record_count, 2);

        // 2. Second sync with unchanged files writes zero packs and zero inbox rows.
        let second = archive.sync_refs(&refs).unwrap();
        assert_eq!(second.archived, 0);
        assert_eq!(second.skipped, 2);
        assert_eq!(store.list_pack_manifests().unwrap().len(), 1);

        // 3. Touching mtime without changing content ALSO deduplicates via BLAKE3 content digest!
        let future = SystemTime::now() + Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(&a_path)
            .unwrap()
            .set_modified(future)
            .unwrap();
        let third = archive.sync_refs(&refs).unwrap();
        assert_eq!(
            third.archived, 0,
            "identical native bytes must deduplicate even if mtime moved"
        );
        assert_eq!(third.skipped, 2);
        assert_eq!(store.list_pack_manifests().unwrap().len(), 1);

        // 4. Hydrate payload from sealed pack (verifying BLAKE3 + XXH64) and check content.
        let alpha_bytes = cv_core::custody::read_session_bytes(&archive.home, "claude", "sess-alpha")
            .unwrap()
            .expect("alpha present");
        // A single-file Claude session is held as its own bytes.
        assert_eq!(alpha_bytes, std::fs::read(&a_path).unwrap());

        // 5. `--frozen` allows `entries()`, `cmd_ls`, and `cmd_path` while refusing `sync_refs`.
        {
            let _frozen = cv_core::env::Env::from_vars([
                ("CLUSTERVISION_HOME", world.cv_home.as_os_str()),
                ("CV_FROZEN", std::ffi::OsStr::new("1")),
            ])
            .enter();
            let (rows, packs) = archive.entries().unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(packs, 1);
            cmd_ls(&archive).unwrap();
            cmd_path(&archive).unwrap();
            assert!(archive.sync_refs(&refs).is_err(), "--frozen must refuse archive sync");
        }
    }
}
