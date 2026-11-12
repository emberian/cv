//! Semantic search via model2vec static embeddings + brute-force cosine.
//!
//! model2vec embeds text with a *static* distilled embedding table (no transformer forward pass,
//! no ONNX): tokenize, look up per-token vectors, mean-pool. It's tiny and CPU-fast, which makes
//! "embed every session up front, then cosine-scan in memory" a perfectly tasteful design for the
//! few-thousand-session scale here. No ANN crate required.
//!
//! On first use the model (`MODEL_REPO`) is fetched from the HuggingFace Hub and cached by
//! `hf-hub` under `~/.cache/huggingface`. To pin a local copy (offline), set
//! `CV_SEARCH_MODEL=/path/to/model-dir`.
//!
//! # The store format
//!
//! Vectors are persisted in a small hand-rolled binary layout (fixed-width little-endian f32s)
//! instead of JSON: a query loads ~N·dim·4 bytes and reinterprets them, rather than JSON-parsing
//! hundreds of thousands of ASCII floats. Layout, all integers little-endian:
//!
//! ```text
//! [8]  magic  b"CVEMBED" + the stored-format version byte (cv_core::source::VERSION)
//! u32  dim          (vector dimensionality)
//! u32  count        (number of records)
//! u64  meta_len
//! […]  meta JSON    ({ model, records: [{session, label, preview, source}…] })
//! […]  vectors      (count × dim × f32 LE, record i's vector at offset i·dim)
//! ```
//!
//! The metadata stays JSON (it's small, schema-evolvable via serde defaults); only the bulk —
//! the vectors — is fixed-width. A pre-binary all-JSON store is rejected like any bad magic:
//! `cv index --semantic` rebuilds it.

#[cfg(test)]
#[path = "semantic_actual_probe.rs"]
mod actual_probe;

use crate::Hit;
use anyhow::{bail, Context, Result};
use cv_core::source::{Baseline, Probe, ReadSet};
use model2vec_rs::model::StaticModel;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A small, strong static-embedding model (~30MB, 256-dim). Distilled from a sentence encoder.
const MODEL_REPO: &str = "minishlab/potion-base-8M";

/// Store header magic: `CVEMBED` + the one stored-format version ([`cv_core::source::VERSION`]):
/// a cv that changes what any store means, or what a stored vector embeds
/// ([`crate::semantic_corpus::DocSink`]), bumps it and re-embeds. 5: rows hold the discovered
/// `SessionRef`; 2 re-spelled its fields; 1 was the implicit all-JSON store.
const MAGIC: &[u8; 8] = &{
    let mut magic = *b"CVEMBED\0";
    magic[7] = cv_core::source::VERSION as u8;
    magic
};

/// Whether the store at `path` is of another `CVEMBED` format version: a refresh re-embeds it
/// whole, where a query names the command that rebuilds it. Anything else (this version, or
/// bytes that are no store at all) is for [`load_store_metadata`] to judge.
fn older_format(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok_and(|()| head[..7] == MAGIC[..7] && head != *MAGIC)
}

/// Per-session metadata, persisted as the store's JSON block (everything but the vector).
#[derive(serde::Serialize, serde::Deserialize)]
struct RecordMeta {
    /// The discovered ref, exactly as discovery handed it: the row is fresh while discovery hands
    /// the same one (and its source is unchanged). Hits read their identity, cwd and dates here.
    session: cv_core::SessionRef,
    /// What hits call the session: its own title, else its first prompt.
    label: Option<String>,
    /// A short preview of the body, for display in hits.
    preview: String,
    source: Option<Baseline>,
}

/// The JSON block inside the binary store (deserialize side; serialization uses a borrowed twin
/// in [`save_store`] to avoid cloning the records).
#[derive(serde::Deserialize)]
struct MetaBlock {
    model: String,
    model_reads: Option<ReadSet>,
    records: Vec<RecordMeta>,
}

/// The in-memory store: metadata plus one flat `count × dim` vector block (record `i`'s vector is
/// `vectors[i*dim..(i+1)*dim]`).
struct Store {
    model: String,
    model_reads: Option<ReadSet>,
    dim: usize,
    meta: Vec<RecordMeta>,
    vectors: Vec<f32>,
}

impl Store {
    fn vector(&self, i: usize) -> &[f32] {
        &self.vectors[i * self.dim..(i + 1) * self.dim]
    }
}

// ---- store I/O -------------------------------------------------------------------------------

/// Serialize + write the store atomically (temp + rename).
fn save_store(path: &Path, store: &Store) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let meta_json = {
        // Serialize the metadata without cloning the records: a borrowed twin of `MetaBlock`.
        #[derive(serde::Serialize)]
        struct MetaBlockRef<'a> {
            model: &'a str,
            model_reads: &'a Option<ReadSet>,
            records: &'a [RecordMeta],
        }
        serde_json::to_vec(&MetaBlockRef {
            model: &store.model,
            model_reads: &store.model_reads,
            records: &store.meta,
        })
        .context("serializing embedding store metadata")?
    };

    let mut staged = cv_core::publish::Staged::beside(path, "embeddings", 0o666)?;
    let mut out = std::io::BufWriter::new(staged.file());
    out.write_all(MAGIC)?;
    out.write_all(&u32::try_from(store.dim)?.to_le_bytes())?;
    out.write_all(&u32::try_from(store.meta.len())?.to_le_bytes())?;
    out.write_all(&(meta_json.len() as u64).to_le_bytes())?;
    out.write_all(&meta_json)?;
    for vector in &store.vectors {
        out.write_all(&vector.to_le_bytes())?;
    }
    out.flush()?;
    drop(out);
    staged.publish(path, cv_core::publish::Claim::Replace)?;
    Ok(())
}

// Parsing the metadata validates the complete declared binary extent. Vector contents are
// arbitrary f32 bits; defer their ownership until a consumer actually needs retained vectors.
struct VectorBacking {
    input: std::io::BufReader<std::fs::File>,
    bytes: usize,
    file_len: u64,
    path: PathBuf,
}
impl VectorBacking {
    fn check_extent(&self) -> Result<()> {
        anyhow::ensure!(
            self.input.get_ref().metadata()?.len() == self.file_len,
            "embedding store changed length during read: {}",
            self.path.display()
        );
        Ok(())
    }
    fn decode_vectors(mut self) -> Result<Vec<f32>> {
        use std::io::Read;
        let mut vectors = Vec::with_capacity(self.bytes / 4);
        let mut wire = [0u8; 16 * 1024];
        let mut remaining = self.bytes;
        while remaining != 0 {
            let bytes = remaining.min(wire.len());
            self.input
                .read_exact(&mut wire[..bytes])
                .context("reading embedding vectors")?;
            vectors.extend(wire[..bytes].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
            remaining -= bytes;
        }
        let mut tail = [0u8; 1];
        anyhow::ensure!(
            self.input.read(&mut tail)? == 0,
            "embedding store grew during read: {}",
            self.path.display()
        );
        Ok(vectors)
    }
}
#[cfg(not(test))]
impl VectorBacking {
    fn decode(self) -> Result<Vec<f32>> {
        self.decode_vectors()
    }
}
#[cfg(test)]
impl VectorBacking {
    fn decode(self) -> Result<Vec<f32>> {
        VECTOR_DECODES.with(|count| count.set(count.get() + 1));
        self.decode_vectors()
    }
}
#[cfg(test)]
thread_local! { static VECTOR_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn load_store(path: &Path) -> Result<Store> {
    let (mut store, vectors) = load_store_metadata(path)?;
    if let Some(vectors) = vectors {
        store.vectors = vectors.decode()?;
    }
    Ok(store)
}

fn load_store_metadata(path: &Path) -> Result<(Store, Option<VectorBacking>)> {
    use std::io::Read;
    let file = std::fs::File::open(path)
        .with_context(|| format!("reading embedding store {} — run `embed_all` first", path.display()))?;
    let file_len = file.metadata()?.len();
    let mut input = std::io::BufReader::new(file);
    let mut header = [0u8; 24];
    let complete_header = match input.read_exact(&mut header) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => false,
        Err(error) => return Err(error).context("reading embedding store header"),
    };
    if !complete_header || &header[..8] != MAGIC {
        bail!(
            "unrecognized embedding store {} (bad magic) — re-run `cv index --semantic`",
            path.display()
        );
    }
    let dim = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    let meta_len = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let meta_end = 24u64
        .checked_add(meta_len)
        .filter(|&end| end <= file_len)
        .with_context(|| format!("truncated embedding store {}", path.display()))?;
    let expected_bytes = count
        .checked_mul(dim)
        .and_then(|n| n.checked_mul(4))
        .context("embedding store dimensions overflow")?;
    anyhow::ensure!(
        file_len - meta_end == expected_bytes as u64,
        "corrupt embedding store {} (vector length mismatch, dim {dim}) — re-run `cv index --semantic`",
        path.display()
    );
    // Slice parsing avoids serde's per-byte Read overhead. Own only the declared metadata
    // extent, then release its wire buffer before allocating the final vector block.
    let meta: MetaBlock = {
        let mut wire = vec![0u8; usize::try_from(meta_len).context("embedding metadata length overflow")?];
        input
            .read_exact(&mut wire)
            .context("reading embedding store metadata")?;
        serde_json::from_slice(&wire).context("parsing embedding store metadata")?
    };
    if meta.records.len() != count {
        bail!(
            "corrupt embedding store {} ({} records, {} vector bytes, dim {dim}) — re-run `cv index --semantic`",
            path.display(),
            meta.records.len(),
            expected_bytes
        );
    }
    Ok((
        Store {
            model: meta.model,
            model_reads: meta.model_reads,
            dim,
            meta: meta.records,
            vectors: Vec::new(),
        },
        Some(VectorBacking {
            input,
            bytes: expected_bytes,
            file_len,
            path: path.to_owned(),
        }),
    ))
}

// ---- embedding -------------------------------------------------------------------------------

/// The id of the model [`load_model`] would load right now: the `CV_SEARCH_MODEL` override (a
/// local model dir) when set, else [`MODEL_REPO`]. Recorded in the store by [`embed_all`] and
/// compared at query time — query and store vectors must come from the **same** model, or the
/// cosine ranking is cross-space noise.
fn model_id() -> String {
    cv_core::env::var_os("CV_SEARCH_MODEL")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| MODEL_REPO.to_string())
}

/// Resolve cached artifact paths first, then load from that pinned local snapshot. An alias
/// moving between batches cannot silently mix embedding spaces. Merely resolving is offline.
struct ModelSpec {
    key: String,
    path: PathBuf,
    reads: ReadSet,
}
fn resolve_model(download: bool) -> Result<Option<ModelSpec>> {
    let id = model_id();
    let root = if cv_core::env::var_os("CV_SEARCH_MODEL").is_some_and(|v| !v.is_empty()) {
        PathBuf::from(&id)
    } else {
        let repo = hf_hub::Cache::from_env().model(id.clone());
        let cached = ["config.json", "tokenizer.json", "model.safetensors"].map(|name| repo.get(name));
        let paths = if let [Some(config), Some(tokenizer), Some(weights)] = cached {
            [config, tokenizer, weights]
        } else if download {
            let api = hf_hub::api::sync::Api::new().context("initializing embedding model download")?;
            let repo = api.model(id.clone());
            [
                repo.get("config.json")?,
                repo.get("tokenizer.json")?,
                repo.get("model.safetensors")?,
            ]
        } else {
            return Ok(None);
        };
        artifact_root(&paths)?
    };
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving model directory {}", root.display()))?;
    let reads = model_reads(&root)?;
    Ok(Some(ModelSpec {
        key: format!("{id}@{}", root.display()),
        path: root,
        reads,
    }))
}
fn artifact_root(paths: &[PathBuf; 3]) -> Result<PathBuf> {
    let parent = paths[0].parent().context("model config has no directory")?;
    anyhow::ensure!(
        paths.iter().all(|p| p.parent() == Some(parent)),
        "model artifacts resolved to different revisions; retry semantic indexing"
    );
    Ok(parent.to_path_buf())
}

/// The tokens a vector embeds: model2vec's `encode` limit. Before tokenizing it cuts the text to
/// this many times the tokenizer's median token length in characters (`truncate_str`), so a
/// session's vector depends on that many characters of its body and no more.
const INPUT_TOKENS: usize = 512;

/// The characters of a body the model reads ([`INPUT_TOKENS`] × the median byte length of the
/// tokenizer's vocabulary, as model2vec computes it), from the model's `tokenizer.json`.
fn input_chars(model: &Path) -> Result<usize> {
    let path = model.join("tokenizer.json");
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    #[derive(serde::Deserialize)]
    struct Tokenizer {
        model: Model,
    }
    #[derive(serde::Deserialize)]
    struct Model {
        vocab: Vocab,
    }
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Vocab {
        Map(std::collections::HashMap<String, serde::de::IgnoredAny>),
        Pieces(Vec<(String, serde::de::IgnoredAny)>),
    }
    let tokenizer: Tokenizer =
        serde_json::from_slice(&bytes).with_context(|| format!("reading the vocabulary of {}", path.display()))?;
    let mut lens: Vec<usize> = match tokenizer.model.vocab {
        Vocab::Map(map) => map.keys().map(String::len).collect(),
        Vocab::Pieces(pieces) => pieces.iter().map(|(piece, _)| piece.len()).collect(),
    };
    lens.sort_unstable();
    Ok(INPUT_TOKENS * lens.get(lens.len() / 2).copied().unwrap_or(1))
}

/// The characters of each session's body the cached model embeds; `None` without a cached model.
pub fn embedded_chars() -> Result<Option<usize>> {
    resolve_model(false)?.map(|spec| input_chars(&spec.path)).transpose()
}

fn load_model(spec: &ModelSpec) -> Result<StaticModel> {
    anyhow::ensure!(
        spec.reads.fresh(&mut Probe::default()),
        "embedding model artifacts changed before loading"
    );
    let model = StaticModel::from_pretrained(&spec.path, None, None, None).context("loading pinned embedding model")?;
    anyhow::ensure!(
        spec.reads.fresh(&mut Probe::default()),
        "embedding model artifacts changed while loading"
    );
    Ok(model)
}

/// Sessions whose bodies are batched into one `model.encode` call. Bounds how many session body
/// heads are resident at once during embedding.
const EMBED_BATCH: usize = 128;

/// Refresh only sessions whose selected semantic sources have changed.
pub fn embed_all(path: &Path) -> Result<usize> {
    let _sources = cv_core::source::read_scope();
    embed_sessions(path, &cv_core::discover_all())
}

pub fn embed_sessions(path: &Path, refs: &[cv_core::SessionRef]) -> Result<usize> {
    embed_with(path, refs, crate::read_corpus_doc, load_model, save_store)
}

/// Read each changed session's body as far as the model reads it: [`input_chars`] characters,
/// at most four bytes each, so the vector is the one the whole body would give.
fn embed_with(
    path: &Path,
    refs: &[cv_core::SessionRef],
    mut read: impl FnMut(&cv_core::SessionRef, usize) -> Result<(crate::Doc, ReadSet)>,
    mut load: impl FnMut(&ModelSpec) -> Result<StaticModel>,
    save: impl FnMut(&Path, &Store) -> Result<()>,
) -> Result<usize> {
    let _sources = cv_core::source::read_scope();
    if refs.is_empty() {
        return refresh(
            path,
            refs,
            &model_id(),
            ReadSet::default(),
            |r| read(r, 0),
            |_| anyhow::bail!("empty semantic inventory unexpectedly demanded a model"),
            save,
        );
    }
    // No model acquisition is needed to remove the last session or publish an empty inventory.
    let spec = resolve_model(true)?;
    let key = spec.as_ref().map_or_else(model_id, |s| s.key.clone());
    let reads = spec.as_ref().map_or_else(ReadSet::default, |s| s.reads.clone());
    let mut model = None;
    let mut head = None;
    refresh(
        path,
        refs,
        &key,
        reads,
        |r| {
            let bytes = match head {
                Some(bytes) => bytes,
                None => *head.insert(4 * input_chars(&spec.as_ref().context("embedding model is not cached")?.path)?),
            };
            read(r, bytes)
        },
        |bodies| {
            if model.is_none() {
                model = Some(load(spec.as_ref().context("embedding model is not cached")?)?);
            }
            Ok(model
                .as_ref()
                .unwrap()
                .encode_with_args(bodies, Some(INPUT_TOKENS), 1024))
        },
        save,
    )
}

fn model_reads(root: &Path) -> Result<ReadSet> {
    fn visit(path: &Path, depth: usize, reads: &mut ReadSet) -> Result<()> {
        anyhow::ensure!(depth < 32, "local model directory nesting exceeds 32");
        reads.record(path);
        if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                reads.record(&entry.path());
                if entry.file_type()?.is_dir() {
                    visit(&entry.path(), depth + 1, reads)?;
                }
            }
        }
        Ok(())
    }
    let mut reads = ReadSet::default();
    visit(root, 0, &mut reads)?;
    // Directory symlinks are not recursively followed; stamp the model's concrete supported
    // layout files too, so a symlinked 0_StaticEmbedding cannot hide artifact replacement.
    for base in [root.to_path_buf(), root.join("0_StaticEmbedding")] {
        for name in [
            "config.json",
            "config_sentence_transformers.json",
            "tokenizer.json",
            "model.safetensors",
        ] {
            reads.record(&base.join(name));
        }
    }
    // A local model root can be 0_StaticEmbedding with its configuration one directory up.
    // In hub snapshots this parent file is normally absent; its absence is stamped too.
    if let Some(parent) = root.parent() {
        reads.record(&parent.join("config_sentence_transformers.json"));
    }
    Ok(reads)
}

// Semantic identity is harness-qualified. Keep discovery's complete inventory, but plan one
// live copy per identity so reuse, projection and final source validation share the same path.
fn canonical_refs(refs: &[cv_core::SessionRef]) -> Vec<&cv_core::SessionRef> {
    let mut selected: std::collections::BTreeMap<_, &cv_core::SessionRef> = std::collections::BTreeMap::new();
    for r in refs {
        selected
            .entry((r.harness.as_str(), r.id.as_str()))
            .and_modify(|old| {
                // Unique identities need no extra filesystem probe; availability only decides copies.
                if (!r.path.exists())
                    .cmp(&(!old.path.exists()))
                    .then_with(|| r.preferred_copy_cmp(old))
                    .is_lt()
                {
                    *old = r;
                }
            })
            .or_insert(r);
    }
    selected.into_values().collect()
}

// The three injected operations keep refresh/publication tests independent of model downloads.
fn refresh(
    path: &Path,
    refs: &[cv_core::SessionRef],
    model: &str,
    model_reads: ReadSet,
    mut read: impl FnMut(&cv_core::SessionRef) -> Result<(crate::Doc, ReadSet)>,
    mut encode: impl FnMut(&[String]) -> Result<Vec<Vec<f32>>>,
    mut save: impl FnMut(&Path, &Store) -> Result<()>,
) -> Result<usize> {
    let refs = canonical_refs(refs);
    let (mut old, vector_backing) = if path.exists() && !older_format(path) {
        load_store_metadata(path)?
    } else {
        (
            Store {
                model: model.into(),
                model_reads: None,
                dim: 0,
                meta: vec![],
                vectors: vec![],
            },
            None,
        )
    };
    let compatible = old.model == model && old.model_reads.as_ref().is_some_and(|r| r.fresh(&mut Probe::default()));
    // The header's source::VERSION already rejects stores predating unique row publication.
    let lookup: std::collections::HashMap<_, _> = old
        .meta
        .iter()
        .enumerate()
        .map(|(i, r)| ((r.session.harness.as_str(), r.session.id.as_str()), i))
        .collect();
    let mut keep = vec![false; old.meta.len()];
    let mut changed = Vec::new();
    let mut before = Probe::default();
    for &r in &refs {
        let snapshot = before.snapshot(r);
        let reusable = lookup.get(&(r.harness.as_str(), r.id.as_str())).copied().filter(|&i| {
            let m = &old.meta[i];
            // A session of a shared store whose own rows certify unchanged keeps its row.
            compatible
                && m.session == *r
                && m.source
                    .as_ref()
                    .is_some_and(|old| old.fresh(&snapshot, &mut before) || old.rows_fresh(r, &snapshot, &mut before))
        });
        if let Some(i) = reusable {
            keep[i] = true;
        }
        if reusable.is_none() {
            // A shared store's certificate as of this snapshot, before the read.
            let rows = before.rows(r);
            changed.push((r, cv_core::source::Baseline { rows, ..snapshot }));
        }
    }
    drop(lookup);
    if let Some(backing) = vector_backing.as_ref() {
        backing.check_extent()?;
    }
    if changed.is_empty() && keep.iter().all(|&v| v) && compatible {
        return Ok(old.meta.len());
    }
    // Only retained rows need old vectors. Unchanged, all-replaced and empty refreshes
    // validate the binary extent but never decode or allocate its unused float payload.
    if keep.iter().any(|&kept| kept) {
        if let Some(backing) = vector_backing {
            old.vectors = backing.decode()?;
        }
    }
    // Compact retained vectors within their existing allocation, preserving old row order.
    let mut retained = Vec::with_capacity(refs.len());
    for (i, meta) in std::mem::take(&mut old.meta).into_iter().enumerate() {
        if keep[i] {
            let dest = retained.len() * old.dim;
            old.vectors.copy_within(i * old.dim..(i + 1) * old.dim, dest);
            retained.push(meta);
        }
    }
    old.vectors.truncate(retained.len() * old.dim);
    old.meta = retained;
    if !compatible {
        old.dim = 0;
    }
    old.model = model.into();
    old.model_reads = Some(model_reads);
    let mut pending = Vec::new();
    let mut bodies = Vec::new();
    for (r, snapshot) in changed {
        let (doc, reads) = read(r)?;
        pending.push(RecordMeta {
            session: r.clone(),
            label: doc.title,
            preview: preview(&doc.body, 200),
            source: Some(snapshot.with_reads(&reads)),
        });
        bodies.push(doc.body);
        if bodies.len() >= EMBED_BATCH {
            encode_batch(&mut encode, &mut pending, &mut bodies, &mut old)?;
        }
    }
    encode_batch(&mut encode, &mut pending, &mut bodies, &mut old)?;
    // A new operation-local probe must re-stat: the pre-parse probe deliberately memoizes.
    let mut after = Probe::default();
    let rows: std::collections::HashMap<_, _> = old
        .meta
        .iter()
        .map(|m| ((m.session.harness.as_str(), m.session.id.as_str()), m))
        .collect();
    for &r in &refs {
        let row = rows
            .get(&(r.harness.as_str(), r.id.as_str()))
            .context("semantic session missing after refresh")?;
        anyhow::ensure!(
            row.source.as_ref().is_some_and(|s| {
                let now = after.snapshot(r);
                s.fresh(&now, &mut after) || s.rows_fresh(r, &now, &mut after)
            }),
            "semantic source changed during refresh: {}",
            r.id
        );
    }
    anyhow::ensure!(
        old.model_reads.as_ref().unwrap().fresh(&mut Probe::default()),
        "model changed during semantic refresh"
    );
    save(path, &old)?;
    Ok(old.meta.len())
}

fn encode_batch(
    encode: &mut impl FnMut(&[String]) -> Result<Vec<Vec<f32>>>,
    pending: &mut Vec<RecordMeta>,
    bodies: &mut Vec<String>,
    store: &mut Store,
) -> Result<()> {
    if bodies.is_empty() {
        return Ok(());
    }
    let vectors = encode(bodies)?;
    anyhow::ensure!(
        vectors.len() == pending.len(),
        "embedding model returned wrong record count"
    );
    for (rec, vector) in pending.drain(..).zip(vectors) {
        if store.dim == 0 {
            store.dim = vector.len();
        }
        anyhow::ensure!(
            vector.len() == store.dim && store.dim != 0,
            "embedding model returned inconsistent dimensions"
        );
        store.vectors.extend_from_slice(&vector);
        store.meta.push(rec);
    }
    bodies.clear();
    Ok(())
}

// ---- query -----------------------------------------------------------------------------------

/// Embed `query` and return the top-`k` sessions by cosine similarity.
///
/// Stores without recorded model artifact identities must refresh once through
/// `cv index --semantic` before querying.
///
/// The store's recorded model must match the model the query would embed under (including a
/// `CV_SEARCH_MODEL` override), and the query vector's dimensionality must match the store's —
/// cosine across different embedding spaces is noise, so a mismatch is an error with a re-embed
/// hint rather than silently garbage rankings.
pub fn semantic_search(path: &Path, query: &str, k: usize) -> Result<Vec<Hit>> {
    semantic_search_filtered(path, query, k, crate::SearchFilter::default())
}

/// Filter stored session metadata before selecting the top semantic results.
pub fn semantic_search_filtered(
    path: &Path,
    query: &str,
    k: usize,
    filter: crate::SearchFilter<'_>,
) -> Result<Vec<Hit>> {
    SemanticSearch::new(Some(path.to_path_buf())).search(query, k, filter)
}

/// Service-owned semantic state. Construction reads no store/model; the first semantic request
/// loads the store/model. One generation is retained and requests share it under a mutex.
/// Dropping this handle releases it; one-shot CLI callers keep their existing stateless path.
pub struct SemanticSearch {
    inner: ResidentSearch<StaticModel>,
}
impl SemanticSearch {
    pub fn new(path: impl Into<Option<PathBuf>>) -> Self {
        Self {
            inner: ResidentSearch::new(path.into().unwrap_or_else(crate::default_embeddings_path)),
        }
    }
    pub fn search(&self, query: &str, k: usize, filter: crate::SearchFilter<'_>) -> Result<Vec<Hit>> {
        Ok(self.search_counted(query, k, filter)?.0)
    }

    /// [`search`](Self::search), and how many stored sessions the filter admits: every one of
    /// them is ranked, so that is the number of matches the top `k` are drawn from.
    pub fn search_counted(&self, query: &str, k: usize, filter: crate::SearchFilter<'_>) -> Result<(Vec<Hit>, usize)> {
        self.search_counted_allowed(query, k, filter, None)
    }

    pub(crate) fn search_counted_allowed(
        &self,
        query: &str,
        k: usize,
        filter: crate::SearchFilter<'_>,
        allowed_ids: Option<&std::collections::HashSet<String>>,
    ) -> Result<(Vec<Hit>, usize)> {
        self.inner.search_allowed(
            query,
            k,
            filter,
            allowed_ids,
            || resolve_model(false),
            load_model,
            |model, query| model.encode_single(query),
        )
    }
}
impl Default for SemanticSearch {
    fn default() -> Self {
        Self::new(None)
    }
}

struct CachedStore {
    store: Store,
    reads: ReadSet,
}
struct CachedModel<M> {
    model: M,
    key: String,
    reads: ReadSet,
}
struct ResidentState<M> {
    path: PathBuf,
    store: Option<CachedStore>,
    model: Option<CachedModel<M>>,
}
impl<M> ResidentState<M> {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            store: None,
            model: None,
        }
    }
    fn clear(&mut self) {
        self.store = None;
        self.model = None;
    }
    #[allow(clippy::too_many_arguments)]
    fn search(
        &mut self,
        query: &str,
        k: usize,
        filter: crate::SearchFilter<'_>,
        allowed_ids: Option<&std::collections::HashSet<String>>,
        resolve: impl FnOnce() -> Result<Option<ModelSpec>>,
        load: impl FnOnce(&ModelSpec) -> Result<M>,
        encode: impl FnOnce(&M, &str) -> Vec<f32>,
    ) -> Result<(Vec<Hit>, usize)> {
        let path = &self.path;
        let mut before = Probe::default();
        if self
            .store
            .as_ref()
            .is_none_or(|cached| !cached.reads.fresh(&mut before))
        {
            // Release the old generation before loading another; a failed replacement never
            // leaves an old answer available as fallback.
            self.store = None;
            let mut reads = ReadSet::default();
            reads.record(path);
            let mut store = load_store(path)?;
            anyhow::ensure!(
                reads.fresh(&mut Probe::default()),
                "embedding store changed while loading: {}",
                path.display()
            );
            // Refresh provenance is validated by the cold decoder but has no consumer in
            // resident ranking. Keep hit fields and model guards, not unused source witnesses.
            for record in &mut store.meta {
                record.source = None;
                record.session.title = None;
                record.session.path = PathBuf::new();
            }
            self.store = Some(CachedStore { store, reads });
        }
        let cached = self.store.as_ref().unwrap();
        if cached.store.meta.is_empty() {
            self.model = None;
            anyhow::ensure!(
                cached.reads.fresh(&mut Probe::default()),
                "embedding store changed during search: {}",
                path.display()
            );
            return Ok((Vec::new(), 0));
        }
        // Resolution checks paths/revisions and artifact metadata, never model bodies.
        let spec = resolve()?.context("embedding model is not cached — run cv index --semantic")?;
        validate_store_model(&cached.store, &spec, path)?;
        if self
            .model
            .as_ref()
            .is_none_or(|model| model.key != spec.key || !model.reads.fresh(&mut before))
        {
            self.model = None;
            let model = load(&spec)?;
            anyhow::ensure!(
                spec.reads.fresh(&mut Probe::default()),
                "embedding model artifacts changed while loading"
            );
            self.model = Some(CachedModel {
                model,
                key: spec.key.clone(),
                reads: spec.reads.clone(),
            });
        }
        let q = encode(&self.model.as_ref().unwrap().model, query);
        anyhow::ensure!(
            q.len() == cached.store.dim,
            "query embedding has dim {}, but store {} has dim {} — run `cv index --semantic` to re-embed",
            q.len(),
            path.display(),
            cached.store.dim
        );
        let hits = rank_filtered_allowed(&cached.store, &q, k, filter, allowed_ids);
        let admitted = cached
            .store
            .meta
            .iter()
            .filter(|meta| admits_allowed(filter, allowed_ids, meta))
            .count();
        let mut after = Probe::default();
        anyhow::ensure!(
            cached.reads.fresh(&mut after),
            "embedding store changed during search: {}",
            path.display()
        );
        anyhow::ensure!(
            spec.reads.fresh(&mut after),
            "embedding model artifacts changed during search"
        );
        Ok((hits, admitted))
    }
}
struct ResidentSearch<M> {
    state: std::sync::Mutex<ResidentState<M>>,
}
impl<M> ResidentSearch<M> {
    fn new(path: PathBuf) -> Self {
        // The handle owns one configured location, even if an embedding process later
        // changes its working directory. Missing paths still load lazily on first demand.
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir().map(|cwd| cwd.join(&path)).unwrap_or(path)
        };
        Self {
            state: std::sync::Mutex::new(ResidentState::new(path)),
        }
    }
    #[cfg(test)]
    fn search(
        &self,
        query: &str,
        k: usize,
        filter: crate::SearchFilter<'_>,
        resolve: impl FnOnce() -> Result<Option<ModelSpec>>,
        load: impl FnOnce(&ModelSpec) -> Result<M>,
        encode: impl FnOnce(&M, &str) -> Vec<f32>,
    ) -> Result<(Vec<Hit>, usize)> {
        self.search_allowed(query, k, filter, None, resolve, load, encode)
    }

    #[allow(clippy::too_many_arguments)]
    fn search_allowed(
        &self,
        query: &str,
        k: usize,
        filter: crate::SearchFilter<'_>,
        allowed_ids: Option<&std::collections::HashSet<String>>,
        resolve: impl FnOnce() -> Result<Option<ModelSpec>>,
        load: impl FnOnce(&ModelSpec) -> Result<M>,
        encode: impl FnOnce(&M, &str) -> Vec<f32>,
    ) -> Result<(Vec<Hit>, usize)> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.clear();
                self.state.clear_poison();
                state
            }
        };
        let result = state.search(query, k, filter, allowed_ids, resolve, load, encode);
        if result.is_err() {
            state.clear();
        }
        result // The lock is released before HTTP/desktop response rendering.
    }
}

fn validate_store_model(store: &Store, spec: &ModelSpec, path: &Path) -> Result<()> {
    if store.model != spec.key
        || store
            .model_reads
            .as_ref()
            .is_none_or(|reads| !reads.fresh(&mut Probe::default()))
    {
        bail!(
            "embedding store {} was built with model {:?}, but the current model is {:?} \
            — run `cv index --semantic` to re-embed (or set CV_SEARCH_MODEL to match)",
            path.display(),
            store.model,
            spec.key
        );
    }
    Ok(())
}

/// Brute-force cosine ranking of the store against an already-embedded query, mapping the top-`k`
/// records to [`Hit`]s. Split from [`semantic_search`] so the record→hit projection (dates,
/// preview-as-snippet) is testable without loading the embedding model.
#[cfg(test)]
fn rank(store: &Store, q: &[f32], k: usize) -> Vec<Hit> {
    rank_filtered(store, q, k, crate::SearchFilter::default())
}

// The heap root is the worst retained hit. Borrow identity metadata rather than copying it
// for every scored row; only the final requested hits own their public fields.
struct RankedRow<'a> {
    score: f32,
    index: usize,
    meta: &'a RecordMeta,
}
impl PartialEq for RankedRow<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for RankedRow<'_> {}
impl PartialOrd for RankedRow<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for RankedRow<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.meta.session.id.cmp(&other.meta.session.id))
            .then_with(|| {
                self.meta
                    .session
                    .harness
                    .as_str()
                    .cmp(other.meta.session.harness.as_str())
            })
    }
}

#[cfg(test)]
fn ranked_rows<'a>(store: &'a Store, q: &[f32], k: usize, filter: crate::SearchFilter<'_>) -> Vec<RankedRow<'a>> {
    ranked_rows_allowed(store, q, k, filter, None)
}

fn ranked_rows_allowed<'a>(
    store: &'a Store,
    q: &[f32],
    k: usize,
    filter: crate::SearchFilter<'_>,
    allowed_ids: Option<&std::collections::HashSet<String>>,
) -> Vec<RankedRow<'a>> {
    if k == 0 {
        return Vec::new();
    }
    let query = CosineQuery::new(q);
    let mut winners = std::collections::BinaryHeap::new();
    for (index, meta) in store.meta.iter().enumerate() {
        if !admits_allowed(filter, allowed_ids, meta) {
            continue;
        }
        let candidate = RankedRow {
            score: query.score(store.vector(index)),
            index,
            meta,
        };
        if winners.len() < k {
            winners.push(candidate);
        } else if let Some(mut worst) = winners.peek_mut() {
            if candidate < *worst {
                *worst = candidate;
            }
        }
        debug_assert!(winners.len() <= k);
    }
    // Refreshing one source moves its vector row. Resolve ties by stable session identity,
    // so incremental and clean stores return the same ranking and top-k boundary.
    winners.into_sorted_vec()
}

/// Whether `filter` admits a stored session: its harness, its cwd containing the needle, and optional id allowlist.
fn admits_allowed(
    filter: crate::SearchFilter<'_>,
    allowed_ids: Option<&std::collections::HashSet<String>>,
    meta: &RecordMeta,
) -> bool {
    filter.harness.is_none_or(|h| meta.session.harness.as_str() == h)
        && filter.cwd.is_none_or(|needle| {
            meta.session
                .cwd
                .as_ref()
                .is_some_and(|cwd| cwd.to_string_lossy().contains(needle))
        })
        && allowed_ids.is_none_or(|ids| ids.contains(&meta.session.id))
}

#[cfg(test)]
fn rank_filtered(store: &Store, q: &[f32], k: usize, filter: crate::SearchFilter<'_>) -> Vec<Hit> {
    rank_filtered_allowed(store, q, k, filter, None)
}

fn rank_filtered_allowed(
    store: &Store,
    q: &[f32],
    k: usize,
    filter: crate::SearchFilter<'_>,
    allowed_ids: Option<&std::collections::HashSet<String>>,
) -> Vec<Hit> {
    let scored = ranked_rows_allowed(store, q, k, filter, allowed_ids);
    scored
        .into_iter()
        .map(|row| {
            let score = row.score;
            let r = &store.meta[row.index];
            Hit {
                id: r.session.id.clone(),
                harness: r.session.harness.as_str().into(),
                cwd: r.session.cwd.as_ref().map(|p| p.display().to_string()),
                title: r.label.clone(),
                score,
                snippet: r.preview.clone(),
                created_at: r.session.created_at.map(|t| t.timestamp()),
                updated_at: r.session.updated_at.map(|t| t.timestamp()),
                // Semantic store doesn't fold the sub-agent forest (yet); no provenance to carry.
                parent_id: None,
                agent_id: None,
                workflow: None,
                session: None,
            }
        })
        .collect()
}

// The query norm is shared by every candidate. Keep the same f32 accumulation and
// denominator order as the defensive cosine oracle; do not assume persisted vectors are unit.
struct CosineQuery<'a> {
    vector: &'a [f32],
    norm: f32,
}
impl<'a> CosineQuery<'a> {
    fn new(vector: &'a [f32]) -> Self {
        let mut norm = 0.0f32;
        for x in vector {
            norm += x * x;
        }
        Self {
            vector,
            norm: norm.sqrt(),
        }
    }
    fn score(&self, candidate: &[f32]) -> f32 {
        if self.vector.len() != candidate.len() || self.vector.is_empty() {
            return 0.0;
        }
        let mut dot = 0.0f32;
        let mut norm = 0.0f32;
        for (x, y) in self.vector.iter().zip(candidate) {
            dot += x * y;
            norm += y * y;
        }
        if self.norm == 0.0 || norm == 0.0 {
            0.0
        } else {
            dot / (self.norm * norm.sqrt())
        }
    }
}

#[cfg(test)]
/// Cosine similarity. model2vec L2-normalizes by default, so this is ~a dot product, but we
/// normalize defensively so unnormalized vectors still rank sanely.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn preview(s: &str, max_chars: usize) -> String {
    let t: String = s
        .split_whitespace()
        .flat_map(|word| std::iter::once(' ').chain(word.chars()))
        .skip(1)
        .take(max_chars)
        .collect();
    // The established ellipsis predicate uses original text length, not normalized length.
    if s.chars().nth(max_chars).is_some() {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_ref(path: &Path, id: &str) -> cv_core::SessionRef {
        cv_core::SessionRef {
            id: id.into(),
            harness: cv_core::Harness::Claude,
            path: path.into(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: Some(1),
        }
    }
    fn fake_doc(r: &cv_core::SessionRef, dependency: Option<&Path>) -> Result<(crate::Doc, ReadSet)> {
        let mut reads = ReadSet::default();
        let mut body = std::fs::read_to_string(&r.path)?;
        if let Some(path) = dependency {
            reads.record(path);
            if path.exists() {
                body.push_str(&std::fs::read_to_string(path)?);
            }
        }
        Ok((crate::Doc { title: None, body }, reads))
    }
    fn fake_encode(bodies: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(bodies
            .iter()
            .map(|b| vec![b.len() as f32, b.bytes().map(u32::from).sum::<u32>() as f32])
            .collect())
    }

    #[test]
    fn semantic_carrier_guard_survives_cache_and_failed_read_never_encodes() {
        use cv_core::MessageSink;
        let dir = std::env::temp_dir().join(format!("cv-semantic-carrier-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let primary = dir.join("primary.jsonl");
        let backing = dir.join("source.json");
        let conditional = dir.join("database-wal");
        let store_path = dir.join("vectors.bin");
        std::fs::write(&primary, "primary unchanged").unwrap();
        std::fs::write(&backing, serde_json::to_vec(&"guarded body ".repeat(20_000)).unwrap()).unwrap();
        let refs = [fixture_ref(&primary, "a")];
        let source = || {
            let mut reads = ReadSet::default();
            reads.record(&conditional);
            cv_core::json::Json::source_with_reads(&backing, 0, std::fs::metadata(&backing).unwrap().len(), &reads)
                .unwrap()
                .with_view(|view| Ok(view.source_text()?.unwrap()))
                .unwrap()
        };
        let encode_calls = std::cell::Cell::new(0);
        let run = |text: Option<&cv_core::Text>| {
            refresh(
                &store_path,
                &refs,
                "fake",
                ReadSet::default(),
                |r| {
                    let text = text.expect("fresh carrier guard must avoid reading").clone();
                    let mut sink = crate::semantic_corpus::DocSink::new(cv_core::Resolver::new(None), 64 * 1024);
                    let mut message = cv_core::Message::new(cv_core::Role::User);
                    message.content.push(cv_core::Block::Text { text });
                    sink.message(message);
                    if let Some(error) = sink.take_error() {
                        return Err(error);
                    }
                    let (mut doc, _) = fake_doc(r, None)?;
                    doc.body = sink.body.into_string();
                    doc.title = sink.first_prompt;
                    Ok((doc, sink.reads))
                },
                |bodies| {
                    encode_calls.set(encode_calls.get() + 1);
                    fake_encode(bodies)
                },
                save_store,
            )
        };
        let captured = source();
        run(Some(&captured)).unwrap();
        assert_eq!(encode_calls.get(), 1);
        let original = std::fs::read(&store_path).unwrap();
        run(None).unwrap();
        assert_eq!(encode_calls.get(), 1);
        assert_eq!(std::fs::read(&store_path).unwrap(), original);
        std::fs::write(&conditional, "conditional dependency adopted").unwrap();
        assert!(run(Some(&captured)).is_err());
        assert_eq!(
            encode_calls.get(),
            1,
            "failed projection must not demand model encoding"
        );
        assert_eq!(std::fs::read(&store_path).unwrap(), original);
        run(Some(&source())).unwrap();
        assert_eq!(encode_calls.get(), 2);
        run(None).unwrap();
        assert_eq!(encode_calls.get(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cached_model_revisions_and_mutated_artifacts_are_distinct_without_network() {
        let dir = std::env::temp_dir().join(format!("cv-model-identity-{}", uuid::Uuid::new_v4()));
        let cache = hf_hub::Cache::new(dir.clone());
        let repo = cache.model("test/model".into());
        let names = ["config.json", "tokenizer.json", "model.safetensors"];
        for revision in ["first", "second"] {
            let root = dir.join("models--test--model/snapshots").join(revision);
            std::fs::create_dir_all(&root).unwrap();
            for name in names {
                std::fs::write(root.join(name), revision).unwrap();
            }
        }
        repo.create_ref("first").unwrap();
        let first_paths = names.map(|name| repo.get(name).unwrap());
        let first = artifact_root(&first_paths).unwrap();
        let reads = model_reads(&first).unwrap();
        repo.create_ref("second").unwrap();
        let second_paths = names.map(|name| repo.get(name).unwrap());
        let second = artifact_root(&second_paths).unwrap();
        assert_ne!(first, second);
        // Loading the first snapshot remains pinned even if main points elsewhere.
        assert!(reads.fresh(&mut Probe::default()));
        let mixed = [first_paths[0].clone(), second_paths[1].clone(), first_paths[2].clone()];
        assert!(artifact_root(&mixed).is_err());
        let input = dir.join("input.jsonl");
        let store_path = dir.join("vectors.bin");
        std::fs::write(&input, "body").unwrap();
        let refs = [fixture_ref(&input, "a")];
        refresh(
            &store_path,
            &refs,
            "same-revision",
            reads.clone(),
            |r| fake_doc(r, None),
            fake_encode,
            save_store,
        )
        .unwrap();
        refresh(
            &store_path,
            &refs,
            "same-revision",
            reads.clone(),
            |_| panic!("unchanged model must reuse"),
            |_| panic!(),
            save_store,
        )
        .unwrap();
        std::fs::write(first.join("model.safetensors"), "replacement weights").unwrap();
        assert!(!reads.fresh(&mut Probe::default()));
        let count = std::cell::Cell::new(0);
        refresh(
            &store_path,
            &refs,
            "same-revision",
            model_reads(&first).unwrap(),
            |r| {
                count.set(count.get() + 1);
                fake_doc(r, None)
            },
            fake_encode,
            save_store,
        )
        .unwrap();
        assert_eq!(
            count.get(),
            1,
            "artifact replacement invalidates vectors even with the same alias/revision path"
        );
        #[cfg(unix)]
        {
            let target = dir.join("linked-static");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("model.safetensors"), "linked weights").unwrap();
            std::os::unix::fs::symlink(&target, first.join("0_StaticEmbedding")).unwrap();
            let linked_reads = model_reads(&first).unwrap();
            std::fs::write(target.join("model.safetensors"), "changed linked weights").unwrap();
            assert!(!linked_reads.fresh(&mut Probe::default()));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_composes_real_claude_projection_with_fake_embedder() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-compose-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("session.jsonl");
        let path = dir.join("vectors.bin");
        let user = serde_json::json!({"type":"user", "uuid":"u", "sessionId":"a", "message":{
            "role":"user", "content":[{"type":"text", "text":"Inspect the file"}]}});
        let assistant = serde_json::json!({"type":"assistant", "uuid":"v", "parentUuid":"u", "sessionId":"a", "message":{
            "role":"assistant", "content":[{"type":"tool_use", "id":"call", "name":"Edit", "input":{"arg":"x".repeat(200000)}}]}});
        std::fs::write(&input, format!("{user}\n{assistant}\n")).unwrap();
        let refs = [fixture_ref(&input, "a")];
        let count = std::cell::Cell::new(0);
        refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |r| crate::read_corpus_doc(r, 64 * 1024),
            |bodies| {
                count.set(count.get() + bodies.len());
                assert_eq!(bodies[0].len(), 64 * 1024);
                assert!(bodies[0].starts_with("Inspect the file\nEdit {\"arg\":\""));
                fake_encode(bodies)
            },
            save_store,
        )
        .unwrap();
        assert_eq!(count.get(), 1);
        let saved = load_store(&path).unwrap();
        assert_eq!(saved.meta[0].label.as_deref(), Some("Inspect the file"));
        refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |_| panic!("fresh native source should not parse"),
            |_| panic!("fresh native source should not load model"),
            save_store,
        )
        .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Isolated requested-Rust-heap oracle; includes native parsing and atomic fake-vector save.
    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "explicit giant fixture and isolated allocator process required"]
    fn production_semantic_heap_probe() {
        use std::io::Write;
        let input = std::path::PathBuf::from(std::env::var_os("CV_SWEEP_INPUT_PROBE").expect("CV_SWEEP_INPUT_PROBE"));
        let bytes: usize = std::env::var("CV_SWEEP_INPUT_BYTES")
            .expect("CV_SWEEP_INPUT_BYTES")
            .parse()
            .unwrap();
        assert!(bytes >= 64 * 1024);
        let dir = std::env::temp_dir().join(format!("cv-semantic-heap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let mut reports = Vec::new();
        for kind in ["arguments", "plain-text", "escaped-text"] {
            let source = if kind == "arguments" {
                input.clone()
            } else {
                dir.join(format!("{kind}.jsonl"))
            };
            if kind != "arguments" {
                let mut out = std::io::BufWriter::new(std::fs::File::create(&source).unwrap());
                out.write_all(br#"{"type":"user","uuid":"u","sessionId":"perf","message":{"role":"user","content":[{"type":"text","text":""#).unwrap();
                let scalar = if kind == "escaped-text" { "\\u0078" } else { "x" };
                let block = scalar.repeat(8192);
                for _ in 0..bytes / 8192 {
                    out.write_all(block.as_bytes()).unwrap();
                }
                out.write_all(scalar.repeat(bytes % 8192).as_bytes()).unwrap();
                out.write_all(b"\"}]}}\n").unwrap();
                out.flush().unwrap();
            }
            let path = dir.join(format!("{kind}.bin"));
            let mut r = fixture_ref(&source, "perf");
            r.message_count = Some(if kind == "arguments" { 2 } else { 1 });
            let refs = [r];
            let mut calls = 0;
            let baseline = cv_testkit::heap::begin();
            refresh(
                &path,
                &refs,
                "fake",
                ReadSet::default(),
                |r| crate::read_corpus_doc(r, 64 * 1024),
                |bodies| {
                    calls += 1;
                    assert_eq!(bodies.len(), 1);
                    if kind == "arguments" {
                        let prefix = "Inspect the file\nEdit {\"file_path\":\"/perf/a.rs\",\"content\":\"";
                        assert_eq!(bodies[0].len(), 64 * 1024);
                        assert!(bodies[0].starts_with(prefix));
                        assert!(bodies[0][prefix.len()..].bytes().all(|b| b == b'x'));
                    } else {
                        // Checked source text caps decoded bytes, independently of JSON escaping.
                        assert_eq!(bodies[0].len(), 64 * 1024);
                        assert!(bodies[0].bytes().all(|b| b == b'x'));
                    }
                    fake_encode(bodies)
                },
                save_store,
            )
            .unwrap();
            let heap = cv_testkit::heap::end(baseline);
            assert_eq!(calls, 1);
            assert!(
                heap.peak_above_baseline_bytes < 8 * 1024 * 1024,
                "{kind}: {}",
                heap.peak_above_baseline_bytes
            );
            refresh(
                &path,
                &refs,
                "fake",
                ReadSet::default(),
                |_| panic!("unchanged producer must not parse"),
                |_| panic!("unchanged producer must not embed"),
                |_, _| panic!("unchanged vectors must not save"),
            )
            .unwrap();
            reports.push(serde_json::json!({"kind":kind,"decoded_payload_bytes":bytes,"source_bytes":std::fs::metadata(&source).unwrap().len(),"heap":heap}));
        }
        println!(
            "{}",
            serde_json::json!({"semantic_projection_heap":reports,"ceiling_bytes":8 * 1024 * 1024,"model":"fake; no model allocations or downloads"})
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_freshness_compares_exact_reference_hints_not_derived_metadata() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-hints-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("session.jsonl");
        let path = dir.join("vectors.bin");
        std::fs::write(&input, "body").unwrap();
        let mut r = fixture_ref(&input, "a");
        r.cwd = Some("/discovery/cwd".into());
        r.title = Some("discovery title".into());
        r.created_at = chrono::DateTime::from_timestamp(42, 1);
        let count = std::cell::Cell::new(0);
        let run = |r: &cv_core::SessionRef| {
            refresh(
                &path,
                std::slice::from_ref(r),
                "fake",
                ReadSet::default(),
                |r| {
                    count.set(count.get() + 1);
                    let (mut d, reads) = fake_doc(r, None)?;
                    d.title = Some("derived title".into());
                    Ok((d, reads))
                },
                fake_encode,
                save_store,
            )
            .unwrap();
        };
        run(&r);
        assert_eq!(count.get(), 1);
        run(&r);
        assert_eq!(count.get(), 1);
        r.title = Some("new hint".into());
        run(&r);
        assert_eq!(count.get(), 2);
        r.message_count = Some(r.message_count.map_or(1, |n| n + 1));
        run(&r);
        assert_eq!(count.get(), 3);
        r.cwd = Some("/other/hint".into());
        run(&r);
        assert_eq!(count.get(), 4);
        r.created_at = chrono::DateTime::from_timestamp(42, 2);
        run(&r);
        assert_eq!(count.get(), 5);
        r.updated_at = chrono::DateTime::from_timestamp(43, 0);
        run(&r);
        assert_eq!(count.get(), 6);
        run(&r);
        assert_eq!(count.get(), 6);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A store an older cv wrote re-embeds whole on refresh; a query still names the rebuild.
    #[test]
    fn semantic_refresh_replaces_an_older_format_store() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-older-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("session.jsonl");
        let path = dir.join("vectors.bin");
        std::fs::write(&input, "body").unwrap();
        let mut prior = *MAGIC;
        prior[7] -= 1;
        std::fs::write(&path, [&prior[..], &[0; 64]].concat()).unwrap();
        assert!(semantic_search(&path, "query", 1)
            .unwrap_err()
            .to_string()
            .contains("bad magic"));
        let r = fixture_ref(&input, "a");
        let n = refresh(
            &path,
            std::slice::from_ref(&r),
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            fake_encode,
            save_store,
        )
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(load_store(&path).unwrap().meta[0].session, r);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_canonicalizes_duplicate_copies() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-copies-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        std::fs::write(&a, "identical").unwrap();
        std::fs::write(&b, "identical").unwrap();
        let refs = [fixture_ref(&b, "same"), fixture_ref(&a, "same")];
        let store = dir.join("store.bin");
        let read = std::cell::Cell::new(0);
        let encoded = std::cell::Cell::new(0);
        let saved = std::cell::Cell::new(0);
        assert_eq!(
            refresh(
                &store,
                &refs,
                "fake",
                ReadSet::default(),
                |r| {
                    read.set(read.get() + 1);
                    fake_doc(r, None)
                },
                |bodies| {
                    encoded.set(encoded.get() + bodies.len());
                    fake_encode(bodies)
                },
                |p, s| {
                    saved.set(saved.get() + 1);
                    save_store(p, s)
                }
            )
            .unwrap(),
            1
        );
        assert_eq!((read.get(), encoded.get(), saved.get()), (1, 1, 1));
        assert_eq!(load_store(&store).unwrap().meta[0].session.path, a);
        refresh(
            &store,
            &refs,
            "fake",
            ReadSet::default(),
            |_| panic!("warm copies must not read"),
            |_| panic!("warm copies must not encode"),
            |_, _| panic!("warm copies must not save"),
        )
        .unwrap();
        let reversed = [refs[1].clone(), refs[0].clone()];
        refresh(
            &store,
            &reversed,
            "fake",
            ReadSet::default(),
            |_| panic!("reordered copies must not read"),
            |_| panic!("reordered copies must not encode"),
            |_, _| panic!("reordered copies must not save"),
        )
        .unwrap();
        let rebuilt = dir.join("rebuilt.bin");
        refresh(
            &rebuilt,
            &reversed,
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            fake_encode,
            save_store,
        )
        .unwrap();
        assert_eq!(std::fs::read(&store).unwrap(), std::fs::read(rebuilt).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_prefers_live_newest_copy_and_keeps_harness_namespaces() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-copy-selection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        let c = dir.join("c.jsonl");
        std::fs::write(&a, "older").unwrap();
        std::fs::write(&b, "newer divergent content").unwrap();
        std::fs::write(&c, "independent harness").unwrap();
        let mut older = fixture_ref(&a, "same");
        older.harness = cv_core::Harness::OpenClaw;
        older.updated_at = chrono::DateTime::from_timestamp(10, 0);
        let mut newer = fixture_ref(&b, "same");
        newer.harness = cv_core::Harness::OpenClaw;
        newer.created_at = chrono::DateTime::from_timestamp(20, 0); // creation fallback beats older update
        let mut other = fixture_ref(&c, "same");
        other.harness = cv_core::Harness::Gemini;
        let refs = [older, other, newer];
        let store = dir.join("store");
        let run = || {
            refresh(
                &store,
                &refs,
                "fake",
                ReadSet::default(),
                |r| fake_doc(r, None),
                fake_encode,
                save_store,
            )
        };
        assert_eq!(run().unwrap(), 2);
        let stored = load_store(&store).unwrap();
        assert_eq!(
            stored
                .meta
                .iter()
                .find(|m| m.session.harness == cv_core::Harness::OpenClaw)
                .map(|m| &m.session.path),
            Some(&b)
        );
        assert_eq!(
            stored
                .meta
                .iter()
                .find(|m| m.session.harness == cv_core::Harness::OpenClaw)
                .unwrap()
                .preview,
            "newer divergent content"
        );
        let rebuild = dir.join("rebuilt");
        let reversed: Vec<_> = refs.iter().rev().cloned().collect();
        refresh(
            &rebuild,
            &reversed,
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            fake_encode,
            save_store,
        )
        .unwrap();
        assert_eq!(std::fs::read(&store).unwrap(), std::fs::read(rebuild).unwrap());
        std::fs::remove_file(&b).unwrap();
        assert_eq!(run().unwrap(), 2); // Missing preferred copy falls back to the older live copy.
        assert_eq!(
            load_store(&store)
                .unwrap()
                .meta
                .iter()
                .find(|m| m.session.harness == cv_core::Harness::OpenClaw)
                .map(|m| &m.session.path),
            Some(&a)
        );
        refresh(
            &store,
            &refs,
            "fake",
            ReadSet::default(),
            |_| panic!("warm fallback must not read"),
            |_| panic!("warm fallback must not encode"),
            |_, _| panic!("warm fallback must not save"),
        )
        .unwrap();
        let before = std::fs::read(&store).unwrap();
        std::fs::remove_file(&a).unwrap();
        assert!(refresh(
            &store,
            &refs,
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            |_| panic!("all-missing copy must not encode"),
            |_, _| panic!("all-missing copy must not save")
        )
        .is_err());
        assert_eq!(std::fs::read(&store).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_reuses_only_fresh_rows_without_loading_model() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-reuse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        let dep = dir.join("dep");
        let path = dir.join("store.bin");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "bb").unwrap();
        let refs = [fixture_ref(&a, "a"), fixture_ref(&b, "b")];
        let run = |model: &str, count: &std::cell::Cell<usize>| {
            refresh(
                &path,
                &refs,
                model,
                ReadSet::default(),
                |r| {
                    count.set(count.get() + 1);
                    fake_doc(r, Some(&dep))
                },
                fake_encode,
                save_store,
            )
            .unwrap()
        };
        let count = std::cell::Cell::new(0);
        assert_eq!(run("fake", &count), 2);
        assert_eq!(count.get(), 2);
        let initial = std::fs::read(&path).unwrap();
        refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |_| panic!("unchanged rows must not parse"),
            |_| panic!("must not load model"),
            |_, _| panic!("unchanged store must not save"),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        #[cfg(unix)]
        {
            let modified = std::fs::metadata(&a).unwrap().modified().unwrap();
            std::fs::write(&a, "c").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&a)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
            count.set(0);
            run("fake", &count);
            assert_eq!(count.get(), 1, "same-size restored-mtime primary cannot reuse vector");
        }
        std::fs::write(&a, "changed primary").unwrap();
        count.set(0);
        run("fake", &count);
        assert_eq!(count.get(), 1);
        // Absence was recorded before the initial projection, so an adopted sidecar invalidates.
        std::fs::write(&dep, "output").unwrap();
        count.set(0);
        run("fake", &count);
        assert_eq!(count.get(), 2);
        count.set(0);
        run("other", &count);
        assert_eq!(count.get(), 2);
        refresh(
            &path,
            &refs[..1],
            "other",
            ReadSet::default(),
            |_| panic!("removal needs no parsing"),
            |_| panic!("removal needs no model"),
            save_store,
        )
        .unwrap();
        assert_eq!(load_store(&path).unwrap().meta.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_decodes_only_vectors_needed_for_composition() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-deferred-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("vectors.bin");
        let refs: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|id| {
                let input = dir.join(format!("{id}.jsonl"));
                std::fs::write(&input, id).unwrap();
                fixture_ref(&input, id)
            })
            .collect();
        let encode = |bodies: &[String]| -> Result<Vec<Vec<f32>>> {
            Ok(bodies.iter().map(|body| vec![body.len() as f32; 256]).collect())
        };
        refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            encode,
            save_store,
        )
        .unwrap();
        let original = std::fs::read(&path).unwrap();
        VECTOR_DECODES.with(|count| count.set(0));
        refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |_| panic!("unchanged source must not parse"),
            |_| panic!("unchanged must not encode"),
            |_, _| panic!("unchanged must not save"),
        )
        .unwrap();
        assert_eq!(VECTOR_DECODES.with(|count| count.get()), 0);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        // Even unused vector bytes must have the exact declared extent before early return.
        std::fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |_| panic!("corrupt store must fail before parsing"),
            |_| panic!("corrupt store must not encode"),
            |_, _| panic!("corrupt store must not save")
        )
        .is_err());
        assert_eq!(VECTOR_DECODES.with(|count| count.get()), 0);
        std::fs::write(&path, &original).unwrap();
        // Retaining a while changing b needs one decode, preserving the former row's bits.
        std::fs::write(&refs[1].path, "changed body").unwrap();
        refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            encode,
            save_store,
        )
        .unwrap();
        assert_eq!(VECTOR_DECODES.with(|count| count.get()), 1);
        let composed = load_store(&path).unwrap();
        assert!(composed.vector(0).iter().all(|&v| v == 1.0));
        assert!(composed.vector(1).iter().all(|&v| v == 12.0));
        // A new embedding space replaces all rows and has no consumer of old vectors.
        VECTOR_DECODES.with(|count| count.set(0));
        refresh(
            &path,
            &refs,
            "new-model",
            ReadSet::default(),
            |r| fake_doc(r, None),
            encode,
            save_store,
        )
        .unwrap();
        assert_eq!(VECTOR_DECODES.with(|count| count.get()), 0);
        refresh(
            &path,
            &[],
            "new-model",
            ReadSet::default(),
            |_| panic!("empty refresh must not parse"),
            |_| panic!("empty must not encode"),
            save_store,
        )
        .unwrap();
        assert_eq!(VECTOR_DECODES.with(|count| count.get()), 0);
        let empty = load_store(&path).unwrap();
        assert!(empty.meta.is_empty() && empty.vectors.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_failures_leave_old_vectors_and_baselines_atomic() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-failure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("input.jsonl");
        let path = dir.join("store");
        std::fs::write(&input, "before").unwrap();
        let refs = [fixture_ref(&input, "a")];
        refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            fake_encode,
            save_store,
        )
        .unwrap();
        let initial = std::fs::read(&path).unwrap();
        let moved = dir.join("saved-input.jsonl");
        std::fs::rename(&input, &moved).unwrap();
        assert!(refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            |_| panic!("missing selected source must not encode"),
            |_, _| panic!("missing selected source must not save")
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        std::fs::rename(&moved, &input).unwrap();
        std::fs::write(&input, "changed").unwrap();
        assert!(refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            fake_encode,
            |_, _| anyhow::bail!("injected save failure")
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        assert!(refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |r| fake_doc(r, None),
            |_| anyhow::bail!("injected model failure"),
            save_store
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        assert!(refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |r| {
                let doc = fake_doc(r, None)?;
                std::fs::write(&input, "changed during projection")?;
                Ok(doc)
            },
            fake_encode,
            save_store
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        let count = std::cell::Cell::new(0);
        refresh(
            &path,
            &refs,
            "fake",
            ReadSet::default(),
            |r| {
                count.set(count.get() + 1);
                fake_doc(r, None)
            },
            fake_encode,
            save_store,
        )
        .unwrap();
        assert_eq!(count.get(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_refresh_reads_once_and_save_failure_cleans_temp() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-once-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("input.jsonl");
        let path = dir.join("store");
        std::fs::write(&input, "new").unwrap();
        let refs = [fixture_ref(&input, "a")];
        let old = store(vec![(meta("a", None), vec![1., 2.])]);
        save_store(&path, &old).unwrap();
        let count = std::cell::Cell::new(0);
        refresh(
            &path,
            &refs,
            "test",
            ReadSet::default(),
            |r| {
                count.set(count.get() + 1);
                fake_doc(r, None)
            },
            fake_encode,
            save_store,
        )
        .unwrap();
        assert_eq!(count.get(), 1);
        refresh(
            &path,
            &refs,
            "test",
            ReadSet::default(),
            |_| panic!(),
            |_| panic!(),
            save_store,
        )
        .unwrap();
        let blocked = dir.join("directory");
        std::fs::create_dir(&blocked).unwrap();
        assert!(save_store(&blocked, &old).is_err());
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cv-")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn meta(id: &str, dates: Option<(i64, i64)>) -> RecordMeta {
        RecordMeta {
            session: cv_core::SessionRef {
                created_at: dates.and_then(|(c, _)| chrono::DateTime::from_timestamp(c, 0)),
                updated_at: dates.and_then(|(_, u)| chrono::DateTime::from_timestamp(u, 0)),
                ..cv_core::SessionRef::new(id, cv_core::Harness::Claude, "")
            },
            label: Some(format!("title-{id}")),
            preview: format!("preview-{id}"),
            source: None,
        }
    }

    fn store(records: Vec<(RecordMeta, Vec<f32>)>) -> Store {
        let dim = records.first().map(|(_, v)| v.len()).unwrap_or(0);
        let mut s = Store {
            model: "test".into(),
            model_reads: Some(ReadSet::default()),
            dim,
            meta: Vec::new(),
            vectors: Vec::new(),
        };
        for (m, v) in records {
            assert_eq!(v.len(), dim);
            s.vectors.extend_from_slice(&v);
            s.meta.push(m);
        }
        s
    }

    /// The head a vector reads is the model's: 512 tokens × the vocabulary's median byte length
    /// (model2vec's `truncate_str`), read from either vocabulary layout of `tokenizer.json`.
    #[test]
    fn input_chars_is_the_models_input_limit() {
        let dir = cv_testkit::temp_dir("semantic-input-chars");
        for (vocab, median) in [
            (
                serde_json::json!({"a": 0, "bb": 1, "cccc": 2, "dddddd": 3, "eeeeeeeee": 4}),
                4,
            ),
            (serde_json::json!([["a", 0.0], ["bbb", -1.0], ["cc", -2.0]]), 2),
        ] {
            let tokenizer = serde_json::json!({"model": {"type": "WordPiece", "vocab": vocab}});
            std::fs::write(dir.join("tokenizer.json"), tokenizer.to_string()).unwrap();
            assert_eq!(input_chars(&dir).unwrap(), INPUT_TOKENS * median);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn resident_spec(artifact: &Path) -> ModelSpec {
        let mut reads = ReadSet::default();
        reads.record(artifact);
        ModelSpec {
            key: "fake@resident".into(),
            path: artifact.parent().unwrap().into(),
            reads,
        }
    }
    fn write_resident_store(path: &Path, spec: &ModelSpec, flipped: bool) {
        let mut second = meta("b", None);
        second.session.harness = cv_core::Harness::Codex;
        second.session.cwd = Some("/resident".into());
        let mut store = store(vec![
            (meta("a", None), if flipped { vec![0.0, 1.0] } else { vec![1.0, 0.0] }),
            (second, if flipped { vec![1.0, 0.0] } else { vec![0.0, 1.0] }),
        ]);
        store.meta[0].session.path = path.with_extension("source");
        store.meta[0].source = Some(Baseline {
            primary: None,
            stamp: None,
            rows: None,
        });
        store.meta[0].session.title = Some("unused refresh title".repeat(4096));
        store.meta[0].session.message_count = Some(17);
        store.model = spec.key.clone();
        store.model_reads = Some(spec.reads.clone());
        save_store(path, &store).unwrap();
    }
    struct HeldModel(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for HeldModel {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn resident_semantic_reuses_and_invalidates_checked_store_and_model() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let dir = std::env::temp_dir().join(format!("cv-semantic-resident-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("vectors.bin");
        let artifact = dir.join("model");
        std::fs::write(&artifact, "v1").unwrap();
        write_resident_store(&path, &resident_spec(&artifact), false);
        let search = ResidentSearch::new(path.clone());
        let loads = AtomicUsize::new(0);
        let encodes = AtomicUsize::new(0);
        let drops = Arc::new(AtomicUsize::new(0));
        let load = |_: &ModelSpec| -> Result<HeldModel> {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok(HeldModel(drops.clone()))
        };
        let encode = |_: &HeldModel, query: &str| {
            encodes.fetch_add(1, Ordering::Relaxed);
            if query == "vertical" {
                vec![0.0, 1.0]
            } else {
                vec![1.0, 0.0]
            }
        };
        let ask = |query, filter| {
            search
                .search(query, 1, filter, || Ok(Some(resident_spec(&artifact))), load, encode)
                .map(|(hits, _)| hits)
        };
        VECTOR_DECODES.with(|count| count.set(0));
        assert_eq!(ask("horizontal", crate::SearchFilter::default()).unwrap()[0].id, "a");
        assert_eq!(ask("vertical", crate::SearchFilter::default()).unwrap()[0].id, "b");
        assert_eq!(
            ask(
                "horizontal",
                crate::SearchFilter {
                    harness: Some("codex"),
                    cwd: Some("resident"),
                    ..Default::default()
                }
            )
            .unwrap()[0]
                .id,
            "b"
        );
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(encodes.load(Ordering::Relaxed), 3);
        assert_eq!(VECTOR_DECODES.with(|count| count.get()), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert!(search
            .state
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .store
            .meta
            .iter()
            .all(|record| record.session.path.as_os_str().is_empty()
                && record.source.is_none()
                && record.session.title.is_none()));
        // Atomic index publication reloads only the store, retaining the unchanged model.
        write_resident_store(&path, &resident_spec(&artifact), true);
        assert_eq!(ask("horizontal", crate::SearchFilter::default()).unwrap()[0].id, "b");
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(VECTOR_DECODES.with(|count| count.get()), 2);
        // Artifact edits invalidate old vectors even though the store's model key is unchanged.
        std::fs::write(&artifact, "v2 model").unwrap();
        assert!(ask("horizontal", crate::SearchFilter::default()).is_err());
        assert_eq!(encodes.load(Ordering::Relaxed), 4);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        write_resident_store(&path, &resident_spec(&artifact), true);
        assert_eq!(ask("horizontal", crate::SearchFilter::default()).unwrap()[0].id, "b");
        assert_eq!(loads.load(Ordering::Relaxed), 2);
        // Revision/override identity mismatches never reuse a prior model or answer.
        assert!(search
            .search(
                "q",
                1,
                crate::SearchFilter::default(),
                || {
                    let mut other = resident_spec(&artifact);
                    other.key = "other@revision".into();
                    Ok(Some(other))
                },
                load,
                encode
            )
            .is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(ask("horizontal", crate::SearchFilter::default()).unwrap()[0].id, "b");
        // Mid-query index publication rejects the captured answer, then the next query reloads.
        assert!(search
            .search(
                "q",
                1,
                crate::SearchFilter::default(),
                || Ok(Some(resident_spec(&artifact))),
                load,
                |model, query| {
                    let q = encode(model, query);
                    write_resident_store(&path, &resident_spec(&artifact), false);
                    q
                }
            )
            .is_err());
        assert_eq!(ask("horizontal", crate::SearchFilter::default()).unwrap()[0].id, "a");
        // Mid-query artifact changes also reject the answer, not merely the next request.
        assert!(search
            .search(
                "q",
                1,
                crate::SearchFilter::default(),
                || Ok(Some(resident_spec(&artifact))),
                load,
                |model, query| {
                    let q = encode(model, query);
                    std::fs::write(&artifact, "v3 changed model").unwrap();
                    q
                }
            )
            .is_err());
        write_resident_store(&path, &resident_spec(&artifact), false);
        assert_eq!(ask("horizontal", crate::SearchFilter::default()).unwrap()[0].id, "a");
        // Deletion is an invalidation too; absent data cannot keep answering old rows.
        std::fs::remove_file(&path).unwrap();
        assert!(ask("horizontal", crate::SearchFilter::default()).is_err());
        write_resident_store(&path, &resident_spec(&artifact), false);
        assert_eq!(ask("horizontal", crate::SearchFilter::default()).unwrap()[0].id, "a");
        // Malformed replacements fail instead of serving the previously cached valid store.
        std::fs::write(&path, b"bad").unwrap();
        assert!(ask("horizontal", crate::SearchFilter::default()).is_err());
        write_resident_store(&path, &resident_spec(&artifact), false);
        assert_eq!(ask("horizontal", crate::SearchFilter::default()).unwrap()[0].id, "a");
        // A failed cold provider load is retryable and cannot leave a partial resident state.
        search.state.lock().unwrap().clear();
        assert!(search
            .search(
                "q",
                1,
                crate::SearchFilter::default(),
                || Ok(Some(resident_spec(&artifact))),
                |_| -> Result<HeldModel> { anyhow::bail!("model unavailable") },
                |_, _| panic!("failed load must not encode")
            )
            .is_err());
        assert!(search.state.lock().unwrap().store.is_none());
        assert_eq!(ask("horizontal", crate::SearchFilter::default()).unwrap()[0].id, "a");
        drop(search);
        assert_eq!(loads.load(Ordering::Relaxed), drops.load(Ordering::Relaxed));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resident_semantic_anchors_relative_store_location() {
        let path = PathBuf::from("missing-semantic-store");
        let expected = std::env::current_dir().unwrap().join(&path);
        let search = ResidentSearch::<()>::new(path);
        assert_eq!(search.state.lock().unwrap().path, expected);
    }

    #[test]
    fn resident_semantic_concurrent_requests_load_once_and_recover_poison() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let dir = std::env::temp_dir().join(format!("cv-semantic-resident-threads-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("vectors.bin");
        let artifact = dir.join("model");
        std::fs::write(&artifact, "v1").unwrap();
        write_resident_store(&path, &resident_spec(&artifact), false);
        let search = ResidentSearch::new(path);
        let loads = AtomicUsize::new(0);
        let drops = Arc::new(AtomicUsize::new(0));
        let decodes = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let (search, loads, drops, decodes, artifact) = (&search, &loads, &drops, &decodes, &artifact);
                scope.spawn(move || {
                    VECTOR_DECODES.with(|count| count.set(0));
                    let hits = search
                        .search(
                            "q",
                            1,
                            crate::SearchFilter::default(),
                            || Ok(Some(resident_spec(artifact))),
                            |_| {
                                loads.fetch_add(1, Ordering::Relaxed);
                                Ok(HeldModel(drops.clone()))
                            },
                            |_, _| vec![1.0, 0.0],
                        )
                        .unwrap()
                        .0;
                    assert_eq!(hits[0].id, "a");
                    decodes.fetch_add(VECTOR_DECODES.with(|count| count.get()), Ordering::Relaxed);
                });
            }
        });
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(decodes.load(Ordering::Relaxed), 1);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = search.search(
                "q",
                1,
                crate::SearchFilter::default(),
                || Ok(Some(resident_spec(&artifact))),
                |_| -> Result<HeldModel> { panic!("cached model should be reused") },
                |_, _| panic!("provider panic"),
            );
        }));
        assert!(panic.is_err());
        let hits = search
            .search(
                "q",
                1,
                crate::SearchFilter::default(),
                || Ok(Some(resident_spec(&artifact))),
                |_| {
                    loads.fetch_add(1, Ordering::Relaxed);
                    Ok(HeldModel(drops.clone()))
                },
                |_, _| vec![0.0, 1.0],
            )
            .unwrap()
            .0;
        assert_eq!(hits[0].id, "b");
        assert_eq!(loads.load(Ordering::Relaxed), 2);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(!search.state.is_poisoned());
        drop(search);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_preview_bounds_normalization_without_changing_wire_text() {
        let oracle = |s: &str, max_chars| {
            let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
            let head: String = joined.chars().take(max_chars).collect();
            if s.chars().count() > max_chars {
                format!("{head}…")
            } else {
                head
            }
        };
        for text in [
            String::new(),
            " \n\t".repeat(3000),
            "a  b\n c".into(),
            "é🙂\u{2003}雪 ".repeat(3000),
            "word".repeat(17000),
            "a ".repeat(17000),
        ] {
            for limit in [0, 1, 2, 5, 200, 1024] {
                assert_eq!(preview(&text, limit), oracle(&text, limit));
                assert!(preview(&text, limit).chars().count() <= limit + 1);
            }
        }
    }

    #[test]
    fn shared_query_norm_preserves_exact_defensive_cosine() {
        let mut vectors = vec![
            vec![],
            vec![0.0, -0.0],
            vec![1.0, 0.0],
            vec![3.0, -4.0],
            vec![f32::NAN, 1.0],
            vec![f32::from_bits(0xffc01234), 0.0],
            vec![f32::INFINITY, 1.0],
            vec![f32::NEG_INFINITY, 2.0],
            vec![f32::MIN_POSITIVE, f32::from_bits(1)],
            vec![f32::MAX, -f32::MAX],
        ];
        for dim in [1, 3, 16, 257] {
            for seed in [1u32, 17, 4129] {
                vectors.push(
                    (0..dim)
                        .map(|i| f32::from_bits((i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed)))
                        .collect(),
                );
            }
        }
        for query in &vectors {
            let shared = CosineQuery::new(query);
            for candidate in &vectors {
                assert_eq!(shared.score(candidate).to_bits(), cosine(query, candidate).to_bits());
            }
        }
    }

    #[test]
    fn bounded_semantic_ranking_matches_exhaustive_order() {
        let records = || {
            (0..4096)
                .map(|i| {
                    let mut record = meta(&format!("session-{:04}", (i * 37) % 4096), None);
                    record.session.harness = if i % 3 == 0 {
                        cv_core::Harness::Codex
                    } else {
                        cv_core::Harness::Claude
                    };
                    record.session.cwd = Some(format!("/repo/{}", i % 7).into());
                    let vector = match i % 19 {
                        0 => vec![0.0, 0.0],
                        1 => vec![f32::NAN, 1.0],
                        2 => vec![f32::INFINITY, 1.0],
                        _ => vec![(i % 23) as f32 - 11.0, (i % 11) as f32 - 5.0],
                    };
                    (record, vector)
                })
                .collect::<Vec<_>>()
        };
        for reverse in [false, true] {
            let store = store(if reverse {
                records().into_iter().rev().collect()
            } else {
                records()
            });
            for filter in [
                crate::SearchFilter::default(),
                crate::SearchFilter {
                    harness: Some("codex"),
                    cwd: Some("/repo/2"),
                    ..Default::default()
                },
            ] {
                let mut oracle: Vec<_> = store
                    .meta
                    .iter()
                    .enumerate()
                    .filter(|(_, record)| {
                        filter.harness.is_none_or(|h| record.session.harness.as_str() == h)
                            && filter.cwd.is_none_or(|needle| {
                                record
                                    .session
                                    .cwd
                                    .as_ref()
                                    .is_some_and(|cwd| cwd.to_string_lossy().contains(needle))
                            })
                    })
                    .map(|(i, record)| {
                        (
                            cosine(&[1.0, 0.0], store.vector(i)),
                            record.session.id.as_str(),
                            record.session.harness.as_str(),
                        )
                    })
                    .collect();
                oracle.sort_unstable_by(|a, b| {
                    b.0.total_cmp(&a.0)
                        .then_with(|| a.1.cmp(b.1))
                        .then_with(|| a.2.cmp(b.2))
                });
                for limit in [0, 1, 7, 64, 4096, usize::MAX] {
                    let ranked = ranked_rows(&store, &[1.0, 0.0], limit, filter);
                    assert!(ranked.len() <= limit);
                    let actual: Vec<_> = ranked
                        .iter()
                        .map(|row| {
                            (
                                row.score.to_bits(),
                                row.meta.session.id.as_str(),
                                row.meta.session.harness.as_str(),
                            )
                        })
                        .collect();
                    let expected: Vec<_> = oracle
                        .iter()
                        .take(limit)
                        .map(|row| (row.0.to_bits(), row.1, row.2))
                        .collect();
                    assert_eq!(actual, expected);
                }
            }
        }
        // No vector access is needed for zero demand, even if the caller's store is incomplete.
        let mut incomplete = store(vec![(meta("unused", None), vec![1.0, 0.0])]);
        incomplete.vectors.clear();
        assert!(ranked_rows(&incomplete, &[1.0, 0.0], 0, crate::SearchFilter::default()).is_empty());
    }

    #[test]
    fn rank_ties_do_not_depend_on_vector_row_order() {
        let records = || {
            let mut codex = meta("a", None);
            codex.session.harness = cv_core::Harness::Codex;
            vec![
                (meta("b", None), vec![1.0, 0.0]),
                (codex, vec![1.0, 0.0]),
                (meta("a", None), vec![1.0, 0.0]),
            ]
        };
        let forward = store(records());
        let reverse = store(records().into_iter().rev().collect());
        for limit in 0..=3 {
            let identities = |store: &Store| {
                rank(store, &[1.0, 0.0], limit)
                    .into_iter()
                    .map(|hit| (hit.id, hit.harness))
                    .collect::<Vec<_>>()
            };
            assert_eq!(identities(&forward), identities(&reverse));
            assert_eq!(
                identities(&forward),
                [("a", "claude"), ("a", "codex"), ("b", "claude")]
                    .into_iter()
                    .take(limit)
                    .map(|(id, harness)| (id.into(), harness.into()))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn rank_filtered_selects_rare_metadata_before_limit() {
        let mut records: Vec<_> = (0..100)
            .map(|i| (meta(&format!("other-{i}"), None), vec![1.0, 0.0]))
            .collect();
        let mut wanted = meta("wanted", None);
        wanted.session.harness = cv_core::Harness::Codex;
        wanted.session.cwd = Some("/repo/mixed.Path/project".into());
        records.push((wanted, vec![0.5, 0.5]));
        let store = store(records);
        for filter in [
            crate::SearchFilter {
                harness: Some("codex"),
                cwd: None,
                ..Default::default()
            },
            crate::SearchFilter {
                harness: None,
                cwd: Some("mixed.Path/proj"),
                ..Default::default()
            },
            crate::SearchFilter {
                harness: Some("codex"),
                cwd: Some("mixed.Path/proj"),
                ..Default::default()
            },
        ] {
            let hits = rank_filtered(&store, &[1.0, 0.0], 1, filter);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].id, "wanted");
            assert_eq!(hits[0].snippet, "preview-wanted");
        }
        assert!(rank_filtered(
            &store,
            &[1.0, 0.0],
            1,
            crate::SearchFilter {
                harness: None,
                cwd: Some("MIXED"),
                ..Default::default()
            }
        )
        .is_empty());
        assert!(rank_filtered(&store, &[1.0, 0.0], 0, crate::SearchFilter::default()).is_empty());
    }

    /// Dates stored on a record surface on its hit; records embedded without dates stay `None`.
    #[test]
    fn rank_carries_dates_onto_hits() {
        let store = store(vec![
            (meta("dated", Some((100, 200))), vec![1.0, 0.0]),
            (meta("dateless", None), vec![0.0, 1.0]),
        ]);
        let hits = rank(&store, &[1.0, 0.0], 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "dated"); // cosine 1.0 ranks first
        assert_eq!(hits[0].created_at, Some(100));
        assert_eq!(hits[0].updated_at, Some(200));
        assert_eq!(hits[0].snippet, "preview-dated");
        assert_eq!(hits[1].id, "dateless");
        assert_eq!(hits[1].created_at, None);
        assert_eq!(hits[1].updated_at, None);
    }

    #[cfg(feature = "probe-heap")]
    #[test]
    #[ignore = "requires isolated allocator process; run with --test-threads=1"]
    fn semantic_store_decode_heap_probe() {
        let dir = std::env::temp_dir().join(format!("cv-emb-load-heap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("embeddings.bin");
        let floats = 1024 * 1024;
        let original = store(vec![(meta("load", None), vec![0.25; floats])]);
        save_store(&path, &original).unwrap();
        drop(original);
        let baseline = cv_testkit::heap::begin();
        let loaded = load_store(&path).unwrap();
        let heap = cv_testkit::heap::end(baseline);
        assert_eq!(loaded.vectors.len(), floats);
        assert!(loaded.vectors.iter().all(|&v| v == 0.25));
        let vector_bytes = floats * std::mem::size_of::<f32>();
        assert!(
            heap.peak_above_baseline_bytes < vector_bytes + 512 * 1024,
            "wire ownership must remain bounded beyond the final vector allocation: {}",
            heap.peak_above_baseline_bytes
        );
        println!(
            "{}",
            serde_json::json!({"semantic_store_decode_heap":heap,"vector_bytes":vector_bytes,"wire_buffer_bytes":16 * 1024})
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn binary_store_streaming_preserves_bits_and_rejects_corruption() {
        let dir = std::env::temp_dir().join(format!("cv-emb-stream-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("embeddings.bin");
        let mut record = meta("escaped", None);
        record.label = Some("\n\"\\雪".repeat(4096));
        // Cross multiple decode-buffer boundaries, retaining every float bit including NaNs.
        let vector: Vec<_> = (0..20_003u32)
            .map(|i| f32::from_bits(i.wrapping_mul(2_654_435_761)))
            .collect();
        let original = store(vec![(record, vector)]);
        save_store(&path, &original).unwrap();
        let encoded = std::fs::read(&path).unwrap();
        let loaded = load_store(&path).unwrap();
        assert_eq!(loaded.meta[0].label, original.meta[0].label);
        assert_eq!(
            loaded.vectors.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
            original.vectors.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
        );
        for corrupt in [
            encoded[..23].to_vec(),
            encoded[..encoded.len() - 1].to_vec(),
            {
                let mut bytes = encoded.clone();
                bytes.push(0);
                bytes
            },
            {
                let mut bytes = encoded.clone();
                bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
                bytes
            },
            {
                let mut bytes = encoded.clone();
                bytes[12..16].copy_from_slice(&2u32.to_le_bytes());
                bytes
            },
            {
                let mut bytes = encoded.clone();
                bytes[24] = b'!';
                bytes
            },
        ] {
            std::fs::write(&path, corrupt).unwrap();
            assert!(load_store(&path).is_err());
        }
        // JSON whitespace belongs to the metadata extent, never the vector decoder.
        let mut spaced = encoded.clone();
        let meta_len = u64::from_le_bytes(spaced[16..24].try_into().unwrap());
        let meta_end = 24 + meta_len as usize;
        spaced.splice(meta_end..meta_end, b" \n\t".iter().copied());
        spaced[16..24].copy_from_slice(&(meta_len + 3).to_le_bytes());
        std::fs::write(&path, spaced).unwrap();
        assert_eq!(load_store(&path).unwrap().vectors.len(), original.vectors.len());
        let empty = store(Vec::new());
        save_store(&path, &empty).unwrap();
        assert!(load_store(&path).unwrap().vectors.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The binary store round-trips exactly: metadata, dim, and every vector bit.
    #[test]
    fn binary_store_roundtrips() {
        let dir = std::env::temp_dir().join(format!("cv-emb-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("embeddings.bin");

        let s = store(vec![
            (meta("a", Some((1, 2))), vec![0.25, -1.5, 3.0]),
            (meta("b", None), vec![f32::MIN_POSITIVE, 0.0, -0.0]),
        ]);
        save_store(&path, &s).unwrap();
        let back = load_store(&path).unwrap();
        assert_eq!(back.model, "test");
        assert_eq!(back.dim, 3);
        assert_eq!(back.meta.len(), 2);
        assert_eq!(back.meta[0].session.id, "a");
        assert_eq!(back.meta[0].session.created_at.map(|t| t.timestamp()), Some(1));
        assert_eq!(back.meta[1].label.as_deref(), Some("title-b"));
        assert_eq!(back.vectors, s.vectors); // bit-exact through to_le_bytes/from_le_bytes
                                             // And the loaded store ranks like the in-memory one.
        assert_eq!(rank(&back, &[0.25, -1.5, 3.0], 1)[0].id, "a");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Garbage on disk fails loudly with a re-index hint instead of mis-parsing.
    #[test]
    fn bad_magic_is_an_error() {
        let dir = std::env::temp_dir().join(format!("cv-emb-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("embeddings.bin");
        std::fs::write(&path, b"not a store at all").unwrap();
        let err = load_store(&path).err().expect("bad magic must fail").to_string();
        assert!(err.contains("bad magic"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
