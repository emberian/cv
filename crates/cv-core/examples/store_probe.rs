//! One explicitly rooted, read-only operation per process. See examples/store_probe.md.

use anyhow::{bail, Context, Result};
use cv_core::{Adapter, Block, Demand, Flow, Harness, Message, Session, SessionRef};
use serde_json::{json, Map, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(feature = "probe-heap")]
use cv_testkit::heap;

#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: heap::Allocator = heap::Allocator;

#[cfg(feature = "sqlite")]
#[path = "store_probe/sqlite_heap.rs"]
mod sqlite_heap;

struct Args {
    operation: String,
    harness: Harness,
    id: String,
    path: PathBuf,
    root: PathBuf,
    mode: Mode,
    materialize: bool,
    span: Option<(u64, u64)>,
    escaped: bool,
    chunk_bytes: usize,
    prefix_bytes: usize,
    last: usize,
    hash_output: bool,
    hash_detail: bool,
    warm_discover: bool,
    query: Option<String>,
    resume_from: Option<PathBuf>,
    raw_order: bool,
    sqlite_heap: bool,
}

/// The `--mode` vocabulary, parsed once so no later stage re-matches raw strings.
#[derive(Clone, Copy)]
enum Mode {
    Lazy,
    LazyExtra,
    Facts,
    Bulk,
    Full,
    Complete,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "lazy" => Mode::Lazy,
            "lazy-extra" => Mode::LazyExtra,
            "facts" => Mode::Facts,
            "bulk" => Mode::Bulk,
            "full" => Mode::Full,
            "complete" => Mode::Complete,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Mode::Lazy => "lazy",
            Mode::LazyExtra => "lazy-extra",
            Mode::Facts => "facts",
            Mode::Bulk => "bulk",
            Mode::Full => "full",
            Mode::Complete => "complete",
        }
    }

    /// The stream demand each mode names.
    fn demand(self) -> Demand {
        match self {
            Mode::Bulk => Demand::LEAN,
            Mode::Full => Demand::RICH,
            Mode::Complete => Demand::COMPLETE,
            Mode::LazyExtra => Demand::RICH.with_spans(),
            Mode::Facts => Demand::FACTS.with_spans(),
            Mode::Lazy => Demand::LEAN.with_spans(),
        }
    }
}

fn args() -> Result<Args> {
    let mut argv = std::env::args().skip(1);
    let mut positional = Vec::new();
    let (mut root, mut materialize) = (None, false);
    let mut mode = "lazy".to_string();
    let (mut span, mut escaped, mut chunk_bytes) = (None, false, 64 << 10);
    let mut prefix_bytes = 64 << 10;
    let mut last = 20;
    let mut hash_output = false;
    let mut hash_detail = false;
    let mut warm_discover = false;
    let mut query = None;
    let mut resume_from = None;
    let mut raw_order = false;
    let mut sqlite_heap = false;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(argv.next().context("--root needs a path")?)),
            "--mode" => mode = argv.next().context("--mode needs lazy|lazy-extra|facts|bulk|full|complete")?,
            "--materialize" => materialize = true,
            "--hash-output" => hash_output = true,
            "--sqlite-heap" => sqlite_heap = true,
            "--prefix-bytes" => prefix_bytes = argv.next().context("--prefix-bytes needs a byte budget")?.parse()?,
            "--hash-detail" => hash_detail = true,
            "--warm-discover" => warm_discover = true,
            "--query" => query = Some(argv.next().context("--query needs an expression")?),
            "--raw-order" => raw_order = true,
            "--span" => {
                let range = argv.next().context("--span needs OFFSET:LENGTH")?;
                let (off, len) = range.split_once(':').context("--span needs OFFSET:LENGTH")?;
                span = Some((off.parse()?, len.parse()?));
            }
            "--escaped" => escaped = true,
            "--chunk-bytes" => chunk_bytes = argv.next().context("--chunk-bytes needs an integer")?.parse()?,
            "--last" => last = argv.next().context("--last needs an integer")?.parse()?,
            "--resume-from" => {
                resume_from = Some(PathBuf::from(
                    argv.next().context("--resume-from needs a checkpoint JSON file")?,
                ))
            }
            "--help" | "-h" => bail!(
                "store_probe OP HARNESS ID PATH [--root DIR] [--mode lazy|lazy-extra|facts|bulk|full|complete] \
                 [--materialize] [--sqlite-heap] [--hash-output] [--hash-detail] [--raw-order] [--warm-discover] [--query EXPR] [--resume-from CHECKPOINT] [--span OFFSET:LENGTH --escaped --chunk-bytes N --prefix-bytes N]\n\
                 OP: baseline|discard|count|checkpoint|resume|collect|window|affordances|affordance-read|doctor|token-stats|dataset|dataset-collect|tools|tools-collect|query|query-collect|hash|discover|sessions|events|events-collected|chunks|prefix|resolve|resolved-len|json-len|json-write|json-prefix\n\
                 HARNESS: any (`harness::for_harness_with_root` names each one's ROOT); use scratch CLUSTERVISION_HOME"
            ),
            s if s.starts_with('-') => bail!("unknown option {s}"),
            _ => positional.push(arg),
        }
    }
    if positional.len() != 4 {
        bail!("expected OP HARNESS ID PATH; see --help");
    }
    let path = std::fs::canonicalize(&positional[3]).context("input path")?;
    let root = root.unwrap_or_else(|| path.parent().unwrap_or(Path::new("/")).to_path_buf());
    let root = std::fs::canonicalize(root).context("adapter root")?;
    let mode = Mode::parse(&mode).with_context(|| format!("unknown mode {mode}"))?;
    if chunk_bytes == 0 {
        bail!("--chunk-bytes must be positive");
    }
    if hash_detail && positional[0] != "hash" {
        bail!("--hash-detail applies only to the hash operation");
    }
    if raw_order && positional[0] != "hash" {
        bail!("--raw-order applies only to the hash operation");
    }
    Ok(Args {
        operation: positional[0].clone(),
        harness: Harness::parse(&positional[1]).context("unknown harness")?,
        id: positional[2].clone(),
        path,
        root,
        mode,
        materialize,
        span,
        escaped,
        chunk_bytes,
        prefix_bytes,
        last,
        hash_output,
        hash_detail,
        warm_discover,
        query,
        resume_from,
        raw_order,
        sqlite_heap,
    })
}

fn adapter(a: &Args) -> Result<Box<dyn Adapter>> {
    cv_core::harness::for_harness_with_root(a.harness, &a.root)
        .context("this harness is not built in (enable the sqlite feature)")
}

// Sort owned maps in place: deterministic JSON without cloning giant payloads into Value trees.
fn sort_map(map: &mut Map<String, Value>) {
    map.sort_keys();
    for v in map.values_mut() {
        match v {
            Value::Object(m) => sort_map(m),
            Value::Array(a) => sort_values(a),
            _ => {}
        }
    }
}

fn sort_values(values: &mut [Value]) {
    for v in values {
        match v {
            Value::Object(m) => sort_map(m),
            Value::Array(a) => sort_values(a),
            _ => {}
        }
    }
}

fn canonical_message(m: &mut Message, resolver: &cv_core::Resolver) -> Result<()> {
    m.materialize(resolver)?;
    sort_map(&mut m.extra);
    for b in &mut m.content {
        match b {
            Block::ToolUse { input, .. } => sort_values(std::slice::from_mut(input.make_owned()?)),
            Block::ToolResult { details: Some(d), .. } => sort_values(std::slice::from_mut(d)),
            _ => {}
        }
    }
    Ok(())
}

#[derive(Default)]
struct DigestWriter {
    digest: cv_core::digest::Sha256,
    bytes: u64,
}

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.digest.update(bytes);
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Counts bytes written without retaining them; the write stays observable to the optimizer.
struct CountWriter(u64);

impl Write for CountWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        std::hint::black_box(buf);
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Outcome {
    messages: usize,
    /// The operation's API does not deliver messages through a callback the probe can count:
    /// `messages` is then reported as null, not as a zero that was never measured.
    unobserved: bool,
    detail: Value,
    // Keep actual collection alive until the stage's heap snapshot is taken.
    session: Option<Session>,
    rows: Vec<cv_core::affordance::Affordance>,
    resolved: Option<String>,
}

fn affordance_detail(rows: &[cv_core::affordance::Affordance], hash_output: bool) -> Result<Value> {
    if !hash_output {
        return Ok(json!({ "affordances": rows.len() }));
    }
    let mut digest = DigestWriter::default();
    for row in rows {
        serde_json::to_writer(&mut digest, row)?;
        digest.write_all(b"\n")?;
    }
    Ok(
        json!({ "affordances": rows.len(), "affordances_sha256": digest.digest.finish_hex(),
               "serialized_affordance_bytes": digest.bytes }),
    )
}

fn run(a: &Args, adapter: &dyn Adapter, r: &SessionRef) -> Result<Outcome> {
    let opts = a.mode.demand();
    let mut out = Outcome::default();
    match a.operation.as_str() {
        "baseline" => {}
        "discover" | "sessions" => {
            let mut refs = if a.operation == "sessions" {
                cv_core::sessions()
            } else {
                adapter.discover()?.refs
            };
            // A store whose count is free (a row count) reports it; one that would have to decode
            // every body reports "not counted" (`None`) until a full read or `cv index` certifies it.
            out.detail = json!({
                "discovered_sessions": refs.len(),
                "discovered_turns": refs.iter().filter_map(|r| r.message_count).sum::<usize>(),
                "uncounted_sessions": refs.iter().filter(|r| r.message_count.is_none()).count(),
            });
            if a.hash_output {
                refs.sort_by(|a, b| (&a.id, &a.path).cmp(&(&b.id, &b.path)));
                let mut digest = DigestWriter::default();
                for r in &refs {
                    serde_json::to_writer(&mut digest, r)?;
                    digest.write_all(b"\n")?;
                }
                out.detail = json!({ "discovered_sessions": refs.len(), "discovery_sha256": digest.digest.finish_hex(),
                                 "serialized_ref_bytes": digest.bytes });
            }
        }
        "events" | "events-collected" => {
            let source = cv_core::source::Probe::default().snapshot(r);
            let mut count = |_m: Message| {
                out.messages += 1;
                Flow::Continue
            };
            let (events, temporary_bytes) = if a.operation == "events" {
                let mut sink = cv_core::events::SpoolingSink::new(r.cwd.clone());
                adapter.try_stream(r, &opts, &mut cv_core::TeeSink::new(&mut count, &mut sink))?;
                let source = source.with_reads(sink.inner().dependencies());
                let spool = sink.into_spool()?;
                let events = spool.len();
                let temporary_bytes = spool.temporary_bytes()?;
                cv_core::events::record_spooled_with_source(r, 0, spool, &source, &Default::default())?;
                (events, temporary_bytes)
            } else {
                let mut sink = cv_core::events::EventSink::new(r.cwd.clone());
                adapter.try_stream(r, &opts, &mut cv_core::TeeSink::new(&mut count, &mut sink))?;
                let source = source.with_reads(sink.dependencies());
                // The retained control: every event collected before one ordered publication.
                let events = sink.into_events()?;
                let mut spool = cv_core::events::EventSpool::default();
                for event in events.iter().cloned() {
                    spool.push(event)?;
                }
                cv_core::events::record_spooled_with_source(r, 0, spool, &source, &Default::default())?;
                (events.len(), 0)
            };
            // The spool reads back exactly what it wrote; only the write is measured.
            out.detail = json!({ "events": events, "temporary_bytes_written": temporary_bytes });
        }
        "checkpoint" | "resume" => {
            // This is a fixture diagnostic, not a replacement for production MarkHead/source
            // checks. The caller owns the append-only prefix contract and compatible sink state.
            let from = match &a.resume_from {
                Some(path) => Some(serde_json::from_slice::<cv_core::stream::Checkpoint>(
                    &std::fs::read(path).context("reading fixture checkpoint")?,
                )?),
                None if a.operation == "resume" => bail!("resume needs --resume-from CHECKPOINT.json"),
                None => None,
            };
            #[derive(Default)]
            struct CheckpointSink {
                messages: usize,
            }
            impl cv_core::MessageSink for CheckpointSink {
                fn message(&mut self, _: Message) -> Flow {
                    self.messages += 1;
                    Flow::Continue
                }
                fn wants_checkpoint(&mut self, _: u64) -> bool {
                    true
                }
            }
            let mut sink = CheckpointSink::default();
            let pass = cv_core::stream::Pass {
                from: from.as_ref(),
                every: Some(a.chunk_bytes as u64),
                ..Default::default()
            };
            let done = cv_core::stream::stream_counted(adapter, r, &opts, &mut sink, pass)?;
            out.messages = sink.messages;
            let counted = done.counted.context("whole diagnostic pass must count")?;
            out.detail = json!({"checkpoint":done.last,"counts":counted.counts,"turns":counted.turns,
                "from_offset":from.as_ref().map_or(0, |cp| cp.offset)});
        }
        "discard" | "count" => {
            let mut counter = cv_core::affordance::Counter::new(r);
            let mut sink = cv_core::stream::TryFnSink::new(|m: Message| {
                out.messages += 1;
                if a.operation == "count" {
                    counter.note(&m)?;
                }
                Ok(Flow::Continue)
            });
            let s = adapter.try_stream(r, &opts, &mut sink)?;
            if a.operation == "count" {
                out.detail = serde_json::to_value(counter.finish(&s, &adapter.store_affordances(r)?))?;
            }
        }
        "tools" | "tools-collect" => {
            let histogram = if a.operation == "tools" {
                cv_core::tools::ToolHistogram::from_stream(adapter, r)?
            } else {
                // The production path before histogram reduction: includes EventSink's unused
                // per-record checkpoints, because the forest reader requested them too.
                let mut sink = cv_core::events::EventSink::new(r.cwd.clone());
                adapter.try_stream(r, &Demand::LEAN.with_spans(), &mut sink)?;
                cv_core::tools::ToolHistogram::from_events(&sink.into_events()?)
            };
            out.detail = serde_json::to_value(histogram)?;
            out.unobserved = true;
        }
        "dataset" | "dataset-collect" => {
            let query = a
                .query
                .as_deref()
                .map(cv_core::SessionQuery::parse)
                .transpose()
                .map_err(anyhow::Error::msg)?;
            let texts = cv_core::query::TextSets::default();
            let mut digest = DigestWriter::default();
            let mut count = CountWriter(0);
            let writer: &mut dyn Write = if a.hash_output { &mut digest } else { &mut count };
            let wrote = if a.operation == "dataset" {
                let record = cv_core::dataset::stage_record(
                    adapter,
                    r,
                    cv_core::dataset::Format::Chatml,
                    None,
                    0,
                    query.as_ref(),
                    &texts,
                )?;
                if let Some(record) = &record {
                    record.publish(writer)?;
                }
                record.is_some()
            } else {
                let session = cv_core::stream::collect_with(adapter, r, &opts)?;
                out.messages = session.messages.len();
                let matched = match query.as_ref() {
                    Some(query) => cv_core::query::matches_full(query, r, &session, &texts)?,
                    None => true,
                };
                let wrote = matched
                    && cv_core::dataset::write_record(&session, &mut *writer, cv_core::dataset::Format::Chatml, None)?;
                if wrote {
                    writer.write_all(b"\n")?;
                }
                out.session = Some(session);
                wrote
            };
            out.detail = json!({"record_written":wrote,
                "serialized_record_bytes":if a.hash_output { digest.bytes } else { count.0 }});
            if a.hash_output {
                out.detail["record_sha256"] = json!(digest.digest.finish_hex());
            }
            if a.operation == "dataset" {
                out.unobserved = true;
            }
        }
        "token-stats" => {
            out.detail = cv_core::stats::TokenStats::compute(std::slice::from_ref(r))?.to_json();
        }
        "doctor" => {
            let mut report = cv_core::doctor::Report::default();
            let mut pass = report.session(Some(r.path.clone()));
            adapter.try_stream(r, &opts, &mut pass)?;
            pass.finish()?;
            out.messages = report.messages as usize;
            out.detail = report.to_json();
        }
        "query" | "query-collect" => {
            let expression = a
                .query
                .as_deref()
                .context("query/query-collect requires --query EXPR")?;
            let query = cv_core::query::SessionQuery::parse(expression).map_err(anyhow::Error::msg)?;
            let texts = cv_core::query::TextSets::default();
            let matched = if a.operation == "query" {
                cv_core::query::matches_streamed(&query, adapter, r, &texts)?
            } else {
                let session = cv_core::stream::collect_with(adapter, r, &Demand::LEAN.with_spans())?;
                let matched = cv_core::query::matches_full(&query, r, &session, &texts)?;
                out.session = Some(session);
                matched
            };
            out.detail = json!({ "matches": matched });
            out.unobserved = true;
        }
        "collect" | "affordances" => {
            let opts = if a.operation == "affordances" {
                Demand::RICH
            } else {
                opts
            };
            let mut sink = cv_core::CollectSink::default();
            let mut s = adapter.try_stream(r, &opts, &mut sink)?;
            s.messages = sink.messages;
            if a.materialize || a.operation == "affordances" {
                s.materialize()?;
            }
            out.messages = s.messages.len();
            if a.operation == "affordances" {
                out.rows = cv_core::affordance::of_session(&s)?;
                out.rows.extend(adapter.store_affordances(r)?);
                out.detail = affordance_detail(&out.rows, a.hash_output)?;
            }
            out.session = Some(s);
        }
        "window" => {
            let mut last = std::collections::VecDeque::new();
            let mut sink = |m: Message| {
                out.messages += 1;
                if a.last > 0 {
                    if last.len() == a.last {
                        last.pop_front();
                    }
                    last.push_back(m);
                }
                Flow::Continue
            };
            let mut s = adapter.try_stream(r, &opts, &mut sink)?;
            s.messages = last.into_iter().collect();
            if a.materialize {
                s.materialize()?;
            }
            out.detail = json!({ "retained_messages": s.messages.len() });
            out.session = Some(s);
        }
        "affordance-read" => {
            out.rows = cv_core::affordance::read(adapter, r)?;
            out.detail = affordance_detail(&out.rows, a.hash_output)?;
            out.unobserved = true;
        }
        "hash" => {
            let resolver = cv_core::Resolver::new(Some(r.path.clone()));
            let mut messages = DigestWriter::default();
            let mut error = None;
            let mut fingerprints = Vec::new();
            let mut sink = |mut m: Message| {
                out.messages += 1;
                let materialized = if a.raw_order {
                    m.materialize(&resolver)
                } else {
                    canonical_message(&mut m, &resolver)
                };
                if let Err(e) = materialized {
                    error = Some(e);
                    return Flow::Stop;
                }
                if let Err(e) = serde_json::to_writer(&mut messages, &m) {
                    error = Some(e.into());
                    return Flow::Stop;
                }
                messages.write_all(b"\n").expect("digest writer");
                if a.hash_detail {
                    let mut one = DigestWriter::default();
                    if let Err(e) = serde_json::to_writer(&mut one, &m) {
                        error = Some(e.into());
                        return Flow::Stop;
                    }
                    one.write_all(b"\n").expect("digest writer");
                    fingerprints.push(json!({ "index_zero_based": out.messages - 1,
                                              "sha256": one.digest.finish_hex(), "serialized_bytes": one.bytes }));
                }
                Flow::Continue
            };
            let mut s = adapter.try_stream(r, &opts, &mut sink)?;
            if let Some(e) = error {
                return Err(e);
            }
            s.messages.clear();
            s.source_path = None; // Transport location, not interpreted session content.
            if !a.raw_order {
                sort_map(&mut s.extra);
            }
            let mut meta = DigestWriter::default();
            serde_json::to_writer(&mut meta, &s)?;
            out.detail = json!({
                "algorithm": "sha256", "canonical_json_version": 1,
                "messages_sha256": messages.digest.finish_hex(),
                "metadata_sha256": meta.digest.finish_hex(),
                "serialized_message_bytes": messages.bytes,
            });
            if a.raw_order {
                out.detail["canonical_json_version"] = Value::Null;
                out.detail["serialization_order"] = json!("preserved");
            }
            if a.hash_detail {
                out.detail["message_fingerprints"] = json!(fingerprints);
                out.detail["metadata"] = serde_json::to_value(&s)?;
            }
        }
        "json-len" | "json-write" | "json-prefix" => {
            let (offset, len) = a
                .span
                .context("json-len/json-write/json-prefix requires --span OFFSET:LENGTH")?;
            // Unlike text spans, this range contains a complete JSON value, including quotes
            // or containers. Construction and strict validation are part of the stage.
            let value = cv_core::Json::source(&r.path, offset, len)?;
            if a.operation == "json-prefix" {
                let (prefix, truncated) = value.canonical_prefix(a.prefix_bytes)?;
                out.detail = json!({ "prefix_bytes": prefix.len(), "prefix_budget_bytes": a.prefix_bytes,
                                     "truncated": truncated });
                if a.hash_output {
                    let mut digest = DigestWriter::default();
                    digest.write_all(prefix.as_bytes())?;
                    out.detail["prefix_sha256"] = json!(digest.digest.finish_hex());
                }
                out.resolved = Some(prefix);
            } else if a.operation == "json-len" {
                out.detail = json!({ "canonical_bytes": value.canonical_len()? });
            } else if a.hash_output {
                let mut digest = DigestWriter::default();
                value.write_canonical(&mut digest)?;
                out.detail = json!({ "canonical_bytes": digest.bytes,
                                     "canonical_sha256": digest.digest.finish_hex() });
            } else {
                let mut writer = CountWriter(0);
                value.write_canonical(&mut writer)?;
                out.detail = json!({ "canonical_bytes": writer.0 });
            }
        }
        "chunks" | "prefix" | "resolve" | "resolved-len" => {
            let (offset, len) = a.span.context("text span operations require --span OFFSET:LENGTH")?;
            if offset
                .checked_add(len)
                .is_none_or(|end| end > std::fs::metadata(&r.path).map_or(0, |m| m.len()))
            {
                bail!("span is outside the source file");
            }
            let span = cv_core::Span {
                source: None,
                offset,
                len,
                escaped: a.escaped,
            };
            let resolver = cv_core::Resolver::new(Some(r.path.clone()));
            if a.operation == "resolved-len" {
                out.detail = json!({ "decoded_bytes": resolver.try_resolved_len(&span)? });
                return Ok(out);
            }
            if a.operation == "prefix" {
                let text = resolver.try_resolve_prefix(&span, a.prefix_bytes as u64)?.into_owned();
                out.detail = json!({ "prefix_bytes": text.len(), "raw_budget_bytes": a.prefix_bytes });
                if a.hash_output {
                    let mut digest = DigestWriter::default();
                    digest.write_all(text.as_bytes())?;
                    out.detail["decoded_sha256"] = json!(digest.digest.finish_hex());
                }
                out.resolved = Some(text);
                return Ok(out);
            }
            if a.operation == "resolve" {
                let text = resolver.try_resolve(&span)?.into_owned();
                out.detail = json!({ "decoded_bytes": text.len() });
                if a.hash_output {
                    let mut digest = DigestWriter::default();
                    digest.write_all(text.as_bytes())?;
                    out.detail["decoded_sha256"] = json!(digest.digest.finish_hex());
                }
                out.resolved = Some(text);
                return Ok(out);
            }
            let (mut chunks, mut bytes, mut max) = (0usize, 0usize, 0usize);
            let mut consume = |text: &str| {
                chunks += 1;
                bytes += text.len();
                max = max.max(text.len());
                std::hint::black_box(text);
            };
            resolver.try_for_each_chunk(&span, a.chunk_bytes, |text| {
                consume(text);
                Ok(())
            })?;
            out.detail = json!({ "chunks": chunks, "decoded_bytes": bytes, "largest_chunk_bytes": max });
        }
        op => bail!("unknown operation {op}"),
    }
    Ok(out)
}

fn main() -> Result<()> {
    let a = args()?;
    #[cfg(feature = "sqlite")]
    let sqlite_accounting = a.sqlite_heap.then(sqlite_heap::Accounting::enable).transpose()?;
    #[cfg(not(feature = "sqlite"))]
    if a.sqlite_heap {
        bail!("--sqlite-heap requires the sqlite feature");
    }
    // Production affordance::read can persist counts, and discovery cache initialization opens
    // a writable catalog. Keep composed operations faithful with an explicitly isolated catalog.
    if a.warm_discover
        || matches!(
            a.operation.as_str(),
            "affordance-read"
                | "discover"
                | "sessions"
                | "events"
                | "events-collected"
                | "query"
                | "query-collect"
                | "token-stats"
                | "dataset"
                | "dataset-collect"
        )
    {
        let home = std::fs::canonicalize(
            std::env::var_os("CLUSTERVISION_HOME").context("this operation requires scratch CLUSTERVISION_HOME")?,
        )
        .context("scratch CLUSTERVISION_HOME must exist")?;
        if let Some(real) = dirs::home_dir().map(|p| p.join(".clustervision")) {
            let real = std::fs::canonicalize(&real).unwrap_or(real);
            if home.starts_with(real) {
                bail!("CLUSTERVISION_HOME must be a scratch catalog, not the real store");
            }
        }
    }
    let adapter = adapter(&a)?;
    // Warm native descriptor/discovery caches in this process, then drop returned refs before
    // the stage. Baseline heap includes any cache state that survives setup; source buffers
    // dropped by discovery are deliberately outside a warm selected-read measurement.
    let setup_discovered_sessions = if a.warm_discover {
        Some(adapter.discover()?.len())
    } else {
        None
    };
    let r = SessionRef::new(&a.id, a.harness, &a.path);
    let physical_bytes = std::fs::metadata(&a.path)?.len();
    #[cfg(feature = "sqlite")]
    let sqlite_baseline = sqlite_accounting.as_ref().map(|counter| counter.begin()).transpose()?;
    #[cfg(feature = "probe-heap")]
    let baseline = heap::begin();
    let start = Instant::now();
    let out = run(&a, adapter.as_ref(), &r)?;
    let seconds = start.elapsed().as_secs_f64();
    #[cfg(feature = "probe-heap")]
    let requested_rust_heap = Some(heap::end(baseline));
    #[cfg(not(feature = "probe-heap"))]
    let requested_rust_heap: Option<()> = None;
    #[cfg(feature = "sqlite")]
    let sqlite_native_heap = sqlite_accounting
        .as_ref()
        .zip(sqlite_baseline)
        .map(|(counter, baseline)| counter.end(baseline))
        .transpose()?;
    #[cfg(not(feature = "sqlite"))]
    let sqlite_native_heap: Option<()> = None;
    println!(
        "{}",
        json!({
            "operation": a.operation, "harness": a.harness.as_str(), "id": a.id,
            "path": a.path, "root": a.root, "requested_mode": a.mode.as_str(),
            "effective_mode": match a.operation.as_str() {
                "affordances" => "full",
                "affordance-read" | "token-stats" => "production-defined",
                "dataset-collect" => a.mode.as_str(),
                "query" | "query-collect" | "tools" | "tools-collect" | "dataset" => "lazy",
                "baseline" | "discover" | "sessions" | "chunks" | "prefix" | "resolve" | "resolved-len" | "json-len" | "json-write" | "json-prefix" => "not-applicable",
                _ => a.mode.as_str(),
            },
            "materialize": if a.operation == "affordance-read" { Value::Null }
                           else { json!(a.materialize || matches!(a.operation.as_str(), "affordances" | "hash" | "resolve")) },
            "physical_input_bytes": physical_bytes,
            "build_features": { "mmap": cfg!(feature = "mmap"), "probe_heap": cfg!(feature = "probe-heap") },
            "hash_output": a.hash_output,
            "hash_detail": a.hash_detail,
            "raw_order": a.raw_order,
            "warm_discover": a.warm_discover,
            "setup_discovered_sessions": setup_discovered_sessions,
            "wall_seconds": seconds, "messages": if out.unobserved { Value::Null } else { json!(out.messages) },
            "requested_rust_heap": requested_rust_heap, "sqlite_native_heap": sqlite_native_heap, "detail": out.detail,
        })
    );
    std::hint::black_box((&out.session, &out.rows, &out.resolved));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_message(mut m: Message, resolver: &cv_core::Resolver) -> String {
        canonical_message(&mut m, resolver).unwrap();
        let mut writer = DigestWriter::default();
        serde_json::to_writer(&mut writer, &m).unwrap();
        writer.digest.finish_hex()
    }

    #[test]
    fn digest_resolves_escaped_spans_and_ignores_json_map_insertion_order() {
        let world = cv_testkit::World::in_process("probe-hash", cv_core::env::Env::enter_isolated);
        let path = world.base.join("input.json");
        let text = "escaped\n\"quote\"\\☃".repeat(1024);
        let quoted = serde_json::to_string(&text).unwrap();
        let raw_input = r#"{"a":0,"z":{"a":1,"b":2}}"#;
        std::fs::write(&path, format!("{quoted}\n{raw_input}")).unwrap();
        let resolver = cv_core::Resolver::new(Some(path.clone()));
        let mut inline = Message::new(cv_core::Role::Assistant);
        inline.content.push(Block::from_text(text.into()));
        inline.content.push(Block::ToolUse {
            id: "call".into(),
            name: "tool".into(),
            namespace: None,
            input: serde_json::from_str(r#"{"z":{"b":2,"a":1},"a":0}"#).unwrap(),
            extensions: Default::default(),
        });
        let mut lazy = inline.clone();
        lazy.content[0] = Block::from_text(cv_core::Text::Span(cv_core::Span {
            source: None,
            offset: 1,
            len: quoted.len() as u64 - 2,
            escaped: true,
        }));
        if let Block::ToolUse { input, .. } = &mut lazy.content[1] {
            *input = cv_core::Json::source(&path, quoted.len() as u64 + 1, raw_input.len() as u64).unwrap();
        }
        let mut stale = lazy.clone();
        assert_eq!(hash_message(inline, &resolver), hash_message(lazy, &resolver));
        std::fs::remove_file(path).unwrap();
        assert!(canonical_message(&mut stale, &resolver).is_err());
    }
}
