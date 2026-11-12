//! clustervision desktop app — the Tauri v2 runtime.
//!
//! This crate is a window around the static web UI (`<repo>/web/`, wired via `frontendDist` in
//! `tauri.conf.json`). Every data path runs in-process without spawning external CLI or HTTP
//! server processes:
//!
//!   1. **One native command, `api(path, query)`**, answering `/api/stats` and `/api/search`
//!      directly via `cv_core` + `cv_search` and delegating every other `/api/*` route to
//!      [`cv_core::api::route_json`].
//!   2. **In-process transcript export (`export_session`)**, staging Markdown and OpenSession 0.3
//!      documents into a private `0600` file and publishing atomically after source verification.
//!   3. **Streaming zip ingest (`File → Open zip…`)**, reading picked `.zip` archives through a
//!      `BufReader<File>` rather than buffering whole archives in RAM, and emitting
//!      `cv://open-sessions` to the webview.
//!   4. **Window state persistence** via `tauri-plugin-window-state`.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Seek, Write};

use serde_json::Value;
use tauri::menu::{AboutMetadataBuilder, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

/// The Tauri event the web UI listens for to load freshly-ingested sessions. Payload is a JSON
/// string holding a `Session[]` array (the same IR the WASM `ingest_zip` returns).
const OPEN_SESSIONS_EVENT: &str = "cv://open-sessions";

/// The public web demo, opened from Help → Open the web demo.
const WEB_DEMO_URL: &str = "https://emberian.github.io/clustervision/";

/// Default `GET /api/search` limit, matching `cv search` and `cv serve`.
const SEARCH_LIMIT: usize = 20;

// ---------------------------------------------------------------------------
// Zip ingest (File → Open zip…)
// ---------------------------------------------------------------------------

/// Stream a seekable `.zip` reader and ingest the contained files into `Session[]` JSON.
fn ingest_zip_reader(reader: impl Read + Seek) -> Result<String, String> {
    let files =
        cv_core::ingest::archive::extract(reader, Default::default()).map_err(|error| format!("zip: {error:#}"))?;
    let sessions = cv_core::ingest::ingest_files(files).map_err(|error| format!("ingesting zip: {error:#}"))?;
    serde_json::to_string(&sessions).map_err(|e| format!("serializing sessions: {e}"))
}

#[cfg(test)]
fn ingest_zip_bytes(bytes: &[u8]) -> Result<String, String> {
    ingest_zip_reader(std::io::Cursor::new(bytes))
}

fn ingest_zip_file(path: &std::path::Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("opening {}: {e}", path.display()))?;
    ingest_zip_reader(BufReader::new(file))
}

// ---------------------------------------------------------------------------
// Reading and searching the machine's real sessions — one in-process command.
// ---------------------------------------------------------------------------

fn resolve_text_sets(query: &cv_core::SessionQuery) -> Result<cv_core::query::TextSets, (u16, String)> {
    use cv_core::query::{FieldId, TextSets};
    let needles = query.needles(FieldId::Text);
    if needles.is_empty() {
        return Ok(TextSets::empty());
    }
    let live = cv_search::LiveRead::of(None).map_err(|e| (503, format!("{e:#}")))?;
    if live == cv_search::LiveRead::Frozen && !cv_search::default_tantivy_dir().exists() {
        return Err((503, "`text:` needs a full-text index; build it with `cv index`".into()));
    }
    let borrowed: Vec<&str> = needles.iter().map(String::as_str).collect();
    let (matches, suffix) = cv_search::text_search_ids_suffixed(None, &borrowed, live, &cv_core::sessions())
        .map_err(|e| (500, format!("opening full-text query index: {e:#}")))?;
    if suffix.no_index && suffix.unread() > 0 {
        return Err((
            503,
            format!(
                "`text:` needs a full-text index: {} of {} session(s) were not read live; build it with `cv index`",
                suffix.unread(),
                suffix.stale
            ),
        ));
    }
    let mut sets = HashMap::new();
    for (needle, result) in needles.into_iter().zip(matches) {
        let ids = result.map_err(|e| (500, format!("resolving text:{needle:?}: {e:#}")))?;
        sets.insert(needle, ids);
    }
    Ok(TextSets::resolve(query, |needle| {
        sets.remove(needle).unwrap_or_default()
    }))
}

fn api_stats(params: &cv_core::api::Query) -> (u16, Value) {
    let q = match params.get("q") {
        Some(s) => match cv_core::SessionQuery::parse(&s) {
            Ok(q) => Some(q),
            Err(e) => {
                return (
                    400,
                    serde_json::json!({ "error": e.replace("`cv query`", "`cv schema`") }),
                )
            }
        },
        None => None,
    };
    let mut refs = cv_core::sessions();
    if let Some(q) = &q {
        let texts = match resolve_text_sets(q) {
            Ok(texts) => texts,
            Err((status, msg)) => return (status, serde_json::json!({ "error": msg })),
        };
        if let Err(error) = cv_core::query::filter_refs(&mut refs, q, &texts) {
            return (500, serde_json::json!({ "error": format!("{error:#}") }));
        }
    }
    (200, cv_core::stats::CorpusStats::compute(&refs).to_json())
}

fn api_search(params: &cv_core::api::Query) -> (u16, Value) {
    let bad = |status: u16, msg: &str| (status, serde_json::json!({ "error": msg }));
    let Some(q) = params.get("q") else {
        return bad(400, "missing q parameter");
    };
    let want = match params.get("harness") {
        Some(h) => match cv_core::Harness::parse(&h) {
            Some(h) => Some(h),
            None => return bad(400, &format!("unknown harness {h:?}")),
        },
        None => None,
    };
    let limit = match params.unsigned("limit") {
        Ok(limit) => limit.unwrap_or(SEARCH_LIMIT),
        Err(error) => return bad(400, &error),
    };
    let cwd = params.get("cwd");
    let flag = |name| {
        params
            .get(name)
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    };
    let (semantic, scan) = (flag("semantic"), flag("scan"));
    if semantic {
        return bad(400, "semantic search is not enabled in the desktop build");
    }
    let scope = match params.get("kind") {
        None => cv_search::Scope::Exchange,
        Some(kind) => match cv_search::Scope::parse(&kind) {
            Some(scope) => scope,
            None => return bad(400, &format!("unknown kind {kind:?}: exchange, context or all")),
        },
    };
    if scan && params.get("kind").is_some() {
        return bad(400, "this mode searches the exchange only; kind= needs the index");
    }
    let source = if scan {
        cv_search::Source::Live
    } else {
        cv_search::Source::Index
    };
    let filter = cv_search::SearchFilter {
        harness: want.map(|h| h.as_str()),
        cwd: cwd.as_deref(),
        scope,
    };
    let live_bytes = match params.unsigned("live_bytes") {
        Ok(bytes) => bytes.map(|b| b as u64),
        Err(error) => return bad(400, &error),
    };
    let searcher = cv_search::Searcher::default();
    let found = match searcher.search(&q, filter, limit, source, live_bytes) {
        Ok(found) => found,
        Err(e) if scan => return bad(500, &format!("live scan failed: {e:#}")),
        Err(e) if cv_search::is_invalid_text_query(&e) => return bad(400, &format!("invalid search query: {e:#}")),
        Err(e) if e.is::<cv_search::BadLiveBytes>() => return bad(500, &format!("{e:#}")),
        Err(e) => {
            return bad(
                503,
                &format!("{e:#} (or add scan=1 for a slow live scan of every session)"),
            )
        }
    };
    let context = cv_core::rows::SearchRowContext::new();
    let rows: Vec<Value> = found.hits.iter().map(|h| cv_search::row(h, &context)).collect();
    (200, Value::Array(rows))
}

/// `GET <path>?<query>` in-process: `/api/stats` and `/api/search` answer directly from
/// `cv_core` + `cv_search`, and every other route delegates to [`cv_core::api::route_json`].
/// A 2xx returns the JSON body as a string; anything else rejects with `"<status>: <error>"`.
#[tauri::command]
fn api(path: String, query: Option<String>) -> Result<String, String> {
    let raw_query = query.as_deref().unwrap_or("");
    let segments: Vec<String> = path
        .split('?')
        .next()
        .unwrap_or("")
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(cv_core::api::decode)
        .collect();
    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();
    let (status, body) = match parts.as_slice() {
        ["api", "stats"] => api_stats(&cv_core::api::Query::parse(raw_query)),
        ["api", "search"] => api_search(&cv_core::api::Query::parse(raw_query)),
        _ => cv_core::api::route_json(&parts, raw_query),
    };
    if (200..300).contains(&status) {
        serde_json::to_string(&body).map_err(|e| format!("serializing {path}: {e}"))
    } else {
        Err(format!(
            "{status}: {}",
            body["error"].as_str().unwrap_or("request failed")
        ))
    }
}

// ---------------------------------------------------------------------------
// Atomic in-process transcript export
// ---------------------------------------------------------------------------

struct ExportStage {
    path: std::path::PathBuf,
    file: std::fs::File,
}
impl ExportStage {
    fn create(parent: &std::path::Path) -> Result<Self, String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let path = parent.join(format!(
                ".cv-export-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => return Ok(Self { path, file }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("staging export: {e}")),
            }
        }
        Err("couldn't create a private export file".into())
    }
}
impl Drop for ExportStage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ExportDestination {
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64, u32),
}
fn export_destination(path: &std::path::Path) -> Result<Option<ExportDestination>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("checking export destination: {e}")),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("export destination must be a regular file, not a symlink or directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("export destination has other hard links; choose a separate file".into());
        }
    }
    Ok(Some(ExportDestination {
        len: metadata.len(),
        modified: metadata
            .modified()
            .map_err(|e| format!("checking destination modification time: {e}"))?,
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.mode(),
            )
        },
    }))
}

fn publish_export(
    destination: &std::path::Path,
    source: &std::path::Path,
    write_fn: impl FnOnce(&mut std::fs::File) -> Result<(), String>,
) -> Result<(), String> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let parent = parent
        .canonicalize()
        .map_err(|e| format!("resolving export directory: {e}"))?;
    let destination = parent.join(destination.file_name().ok_or("export destination has no filename")?);
    let source = source
        .canonicalize()
        .map_err(|e| format!("resolving transcript source: {e}"))?;
    if destination == source || (source.is_dir() && destination.starts_with(&source)) {
        return Err("export destination would replace transcript source; choose a separate file".into());
    }
    let before = export_destination(&destination)?;
    let mut stage = ExportStage::create(&parent)?;
    write_fn(&mut stage.file)?;
    stage.file.sync_all().map_err(|e| format!("finishing export: {e}"))?;
    if export_destination(&destination)? != before {
        return Err("export destination changed during export; choose the destination again".into());
    }
    if before.is_none() {
        // Atomic no-clobber publication: a newly appearing destination is never replaced.
        std::fs::hard_link(&stage.path, &destination).map_err(|e| format!("saving export: {e}"))
    } else {
        std::fs::rename(&stage.path, &destination).map_err(|e| format!("saving export: {e}"))
    }
}

fn write_session_export(
    adapter: &dyn cv_core::Adapter,
    reference: &cv_core::SessionRef,
    format: &str,
    file: &mut std::fs::File,
) -> Result<(), String> {
    let _reads = cv_core::source::read_scope();
    let mut out = BufWriter::with_capacity(64 * 1024, file);
    match format {
        "markdown" => {
            let session = cv_core::collect(adapter, reference).map_err(|e| format!("reading transcript: {e:#}"))?;
            let resolver = session.resolver();
            cv_core::render::write_markdown(&session, &mut out, &resolver)
                .map_err(|e| format!("writing markdown export: {e:#}"))?;
        }
        "opensession" => {
            let mut sink = cv_core::api::WindowSink::new(&reference.path, 0, None);
            let pass = cv_core::stream::Pass {
                guard: true,
                ..cv_core::stream::Pass::RECORD
            };
            let read = cv_core::stream::stream_counted(
                adapter,
                reference,
                &cv_core::Demand::RICH.with_spans(),
                &mut sink,
                pass,
            )
            .map_err(|e| format!("reading transcript: {e:#}"))?;
            let frames = sink.finish().map_err(|e| format!("staging frames: {e:#}"))?;
            let framing = cv_core::projection::Framing {
                pretty: true,
                total: Some(sink.selected),
                ..cv_core::projection::Framing::default()
            };
            cv_core::projection::write_frames(
                &mut out,
                &cv_core::harness::opensession::document(&read.session),
                "messages",
                &frames,
                framing,
            )
            .map_err(|e| format!("writing opensession export: {e:#}"))?;
            out.write_all(b"\n")
                .map_err(|e| format!("writing opensession newline: {e}"))?;
            if let Some(guard) = read.guard {
                guard
                    .verify(reference)
                    .map_err(|e| format!("transcript changed during export: {e:#}"))?;
            }
        }
        _ => return Err("unknown export format".into()),
    }
    out.flush().map_err(|e| format!("flushing export: {e}"))
}

fn export_session_to_path(harness: &str, id: &str, format: &str, destination: &std::path::Path) -> Result<(), String> {
    if !matches!(format, "markdown" | "opensession") {
        return Err("unknown export format".into());
    }
    let kind = cv_core::Harness::parse(harness).ok_or("unknown source harness")?;
    let (reference, adapter) = cv_core::find(id, Some(kind))
        .map_err(|e| format!("finding transcript source: {e}"))?
        .ok_or("transcript source not found")?;
    let primary = cv_core::source::primary_path(&reference);
    publish_export(destination, &primary, |file| {
        write_session_export(adapter.as_ref(), &reference, format, file)
    })
}

#[tauri::command]
async fn export_session(app: AppHandle, harness: String, id: String, format: String) -> Result<bool, String> {
    let extension = match format.as_str() {
        "markdown" => "md",
        "opensession" => "opensession.json",
        _ => return Err("unknown export format".into()),
    };
    let filename = format!(
        "cv-{}.{}",
        id.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            })
            .collect::<String>(),
        extension
    );
    tauri::async_runtime::spawn_blocking(move || {
        let Some(picked) = app
            .dialog()
            .file()
            .set_title("Export complete transcript")
            .set_file_name(filename)
            .blocking_save_file()
        else {
            return Ok(false);
        };
        let destination = picked.into_path().map_err(|e| format!("resolving save path: {e}"))?;
        export_session_to_path(&harness, &id, &format, &destination)?;
        Ok(true)
    })
    .await
    .map_err(|e| format!("export task failed: {e}"))?
}

// ---------------------------------------------------------------------------
// Native application menu
// ---------------------------------------------------------------------------

/// Build the native application menu (File / Edit / View / Help). Menu item IDs are matched in
/// [`handle_menu_event`].
fn build_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    // --- File
    let open_zip = MenuItem::with_id(app, "file.open_zip", "Open zip…", true, Some("CmdOrCtrl+O"))?;
    let quit = PredefinedMenuItem::quit(app, Some("Quit clustervision"))?;
    let file = Submenu::with_items(app, "File", true, &[&open_zip, &quit])?;

    // --- Edit: standard macOS/OS editing actions so Cmd+C/Cmd+V/Cmd+A work inside the webview.
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, Some("Undo"))?,
            &PredefinedMenuItem::redo(app, Some("Redo"))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, Some("Cut"))?,
            &PredefinedMenuItem::copy(app, Some("Copy"))?,
            &PredefinedMenuItem::paste(app, Some("Paste"))?,
            &PredefinedMenuItem::select_all(app, Some("Select All"))?,
        ],
    )?;

    // --- View
    let reload = MenuItem::with_id(app, "view.reload", "Reload", true, Some("CmdOrCtrl+R"))?;
    let devtools = MenuItem::with_id(app, "view.devtools", "Toggle DevTools", true, Some("CmdOrCtrl+Alt+I"))?;
    let fullscreen = PredefinedMenuItem::fullscreen(app, Some("Toggle Fullscreen"))?;
    let view = Submenu::with_items(app, "View", true, &[&reload, &devtools, &fullscreen])?;

    // --- Help
    let about_meta = AboutMetadataBuilder::new()
        .name(Some("clustervision"))
        .version(Some(env!("CARGO_PKG_VERSION")))
        .comments(Some("Cross-harness AI session viewer."))
        .build();
    let about = PredefinedMenuItem::about(app, Some("About clustervision"), Some(about_meta))?;
    let web_demo = MenuItem::with_id(app, "help.web_demo", "Open the web demo", true, None::<&str>)?;
    let help = Submenu::with_items(app, "Help", true, &[&about, &web_demo])?;

    Menu::with_items(app, &[&file, &edit, &view, &help])
}

/// React to a native menu click. Unknown IDs are ignored.
fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    match event.id().as_ref() {
        "file.open_zip" => open_zip_via_dialog(app),
        "view.reload" => {
            if let Some(win) = main_window(app) {
                let _ = win.eval("window.location.reload()");
            }
        }
        "view.devtools" => {
            if let Some(win) = main_window(app) {
                if win.is_devtools_open() {
                    win.close_devtools();
                } else {
                    win.open_devtools();
                }
            }
        }
        "help.web_demo" => {
            #[allow(deprecated)]
            {
                use tauri_plugin_shell::ShellExt;
                let _ = app.shell().open(WEB_DEMO_URL, None);
            }
        }
        _ => {}
    }
}

/// Get the main webview window (label `"main"`), or `None` if it's gone.
fn main_window<R: Runtime>(app: &AppHandle<R>) -> Option<WebviewWindow<R>> {
    app.get_webview_window("main")
}

/// File → Open zip…: native picker → streaming `BufReader<File>` ingest → emit
/// `cv://open-sessions` with the `Session[]` JSON payload.
fn open_zip_via_dialog<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    std::thread::spawn(move || {
        let picked = app
            .dialog()
            .file()
            .add_filter("Harness export (.zip)", &["zip"])
            .set_title("Open a harness export zip")
            .blocking_pick_file();

        let Some(file_path) = picked else {
            return;
        };

        let result = file_path
            .into_path()
            .map_err(|e| format!("resolving picked path: {e}"))
            .and_then(|p| ingest_zip_file(&p));

        match result {
            Ok(sessions_json) => {
                if let Err(e) = app.emit(OPEN_SESSIONS_EVENT, sessions_json) {
                    eprintln!("clustervision: failed to emit {OPEN_SESSIONS_EVENT}: {e}");
                }
            }
            Err(e) => {
                use tauri_plugin_dialog::{MessageDialogButtons, MessageDialogKind};
                app.dialog()
                    .message(e)
                    .kind(MessageDialogKind::Error)
                    .title("Couldn't open zip")
                    .buttons(MessageDialogButtons::Ok)
                    .blocking_show();
            }
        }
    });
}

/// Build and run the Tauri application. Called by `main.rs`.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .invoke_handler(tauri::generate_handler![api, export_session])
        .on_menu_event(handle_menu_event)
        .setup(|app| {
            let menu = build_menu(app.handle())?;
            app.set_menu(menu)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building clustervision")
        .run(|_app_handle, _event| {});
}

// ---------------------------------------------------------------------------
// Tests — cover the native logic (streaming zip ingest, the api command incl.
// /api/stats and /api/search, and atomic in-process export publication).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::Session;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cv-tauri-{label}-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Build an in-memory zip from `(name, bytes)` entries using `Stored`.
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    const CLAUDE_JSONL: &str = concat!(
        r#"{"type":"user","sessionId":"sess-abc","parentUuid":null,"message":{"role":"user","content":"hello needleword"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"sess-abc","parentUuid":null,"message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#,
        "\n",
    );

    #[test]
    fn ingest_zip_streams_from_file_and_bytes() {
        let zip = make_zip(&[("logs/session.jsonl", CLAUDE_JSONL.as_bytes())]);
        let json = ingest_zip_bytes(&zip).expect("ingest should succeed");
        let sessions: Vec<Session> = serde_json::from_str(&json).expect("valid Session[] JSON");
        assert_eq!(sessions.len(), 1, "one transcript → one session");
        assert_eq!(sessions[0].harness, cv_core::Harness::Claude);
        assert!(!sessions[0].messages.is_empty());

        let root = temp_dir("zip-file");
        let zip_path = root.join("upload.zip");
        std::fs::write(&zip_path, &zip).unwrap();
        let streamed_json = ingest_zip_file(&zip_path).expect("streaming file ingest should succeed");
        let streamed: Vec<Session> = serde_json::from_str(&streamed_json).unwrap();
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].id, sessions[0].id);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ingest_zip_bytes_rejects_non_zip() {
        let err = ingest_zip_bytes(b"not a zip at all").unwrap_err();
        assert!(err.contains("not a readable zip"), "got: {err}");
    }

    #[test]
    fn ingest_zip_bytes_reports_transcript_errors() {
        let zip = make_zip(&[("rollout-broken.json", br#"{"session": {}, "messages": ["#)]);
        let error = ingest_zip_bytes(&zip).unwrap_err();
        assert!(error.starts_with("ingesting zip:"), "got: {error}");
    }

    #[test]
    fn ingest_zip_bytes_skips_dirs_and_unknown_files() {
        let zip = make_zip(&[("notes/", b""), ("random.txt", b"just some prose, not a transcript")]);
        let json = ingest_zip_bytes(&zip).expect("ingest should not error on unknown content");
        let sessions: Vec<Session> = serde_json::from_str(&json).unwrap();
        assert!(sessions.is_empty(), "nothing recognizable → []");
    }

    #[test]
    fn api_command_routes_read_api_stats_and_search_in_process() {
        let root = temp_dir("api");
        let home = root.join("home");
        let cv_home = root.join("cv-home");
        let claude_proj = home.join(".claude").join("projects").join("-tmp-proj");
        std::fs::create_dir_all(&claude_proj).unwrap();
        std::fs::create_dir_all(&cv_home).unwrap();
        let transcript = claude_proj.join("sess-abc.jsonl");
        std::fs::write(&transcript, CLAUDE_JSONL).unwrap();

        let _scope = cv_core::env::Env::from_vars([
            ("HOME", home.as_os_str()),
            ("USERPROFILE", home.as_os_str()),
            ("CLUSTERVISION_HOME", cv_home.as_os_str()),
        ])
        .enter();
        {
            let health: Value = serde_json::from_str(&api("/api/health".into(), None).unwrap()).unwrap();
            assert_eq!(health["ok"], true);
            assert!(api("/api/nope".into(), None).unwrap_err().starts_with("404"));
            assert!(api("/api/channels".into(), None).unwrap_err().starts_with("404"));
            assert!(api("/api/sessions".into(), Some("harness=warpdrive".into()))
                .unwrap_err()
                .starts_with("400"));

            // In-process /api/stats
            let stats: Value = serde_json::from_str(&api("/api/stats".into(), None).unwrap()).unwrap();
            assert_eq!(stats["sessions"], 1);
            let filtered: Value =
                serde_json::from_str(&api("/api/stats".into(), Some("q=harness:codex".into())).unwrap()).unwrap();
            assert_eq!(filtered["sessions"], 0);
            assert!(api("/api/stats".into(), Some("q=(unclosed".into()))
                .unwrap_err()
                .starts_with("400"));

            // In-process /api/search (live suffix when no index exists, plus explicit scan=1)
            assert!(api("/api/search".into(), None).unwrap_err().starts_with("400"));
            assert!(api("/api/search".into(), Some("q=needleword&harness=bogus".into()))
                .unwrap_err()
                .starts_with("400"));
            let hits: Vec<Value> =
                serde_json::from_str(&api("/api/search".into(), Some("q=needleword".into())).unwrap()).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0]["id"], "sess-abc");
            let scan_hits: Vec<Value> =
                serde_json::from_str(&api("/api/search".into(), Some("q=needleword&scan=1".into())).unwrap()).unwrap();
            assert_eq!(scan_hits.len(), 1);

            // In-process export_session_to_path (Markdown and OpenSession 0.3)
            let md_dest = root.join("out.md");
            export_session_to_path("claude", "sess-abc", "markdown", &md_dest).unwrap();
            let md = std::fs::read_to_string(&md_dest).unwrap();
            assert!(md.contains("needleword"), "markdown export contains transcript");

            let json_dest = root.join("out.opensession.json");
            export_session_to_path("claude", "sess-abc", "opensession", &json_dest).unwrap();
            let doc: Value = serde_json::from_str(&std::fs::read_to_string(&json_dest).unwrap()).unwrap();
            assert_eq!(doc["open_session"], "0.3");
            assert_eq!(doc["id"], "sess-abc");
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn export_publication_preserves_destination_on_failure() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("export-pub");
        let source = root.join("source.jsonl");
        let destination = root.join("session.json");
        std::fs::write(&source, CLAUDE_JSONL).unwrap();
        std::fs::write(&destination, "previous complete export").unwrap();

        let error = publish_export(&destination, &source, |file| {
            file.write_all(b"partial").unwrap();
            Err("broken source".into())
        })
        .unwrap_err();
        assert!(error.contains("broken source"));
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "previous complete export"
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2, "private stages removed");

        publish_export(&destination, &source, |file| {
            file.write_all(b"complete export").map_err(|e| e.to_string())
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "complete export");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2, "success stages removed");
        assert_eq!(
            std::fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );

        assert!(publish_export(&source, &source, |_| Ok(()))
            .unwrap_err()
            .contains("transcript source"));

        let alias = root.join("alias");
        std::os::unix::fs::symlink(&destination, &alias).unwrap();
        assert!(publish_export(&alias, &source, |_| Ok(()))
            .unwrap_err()
            .contains("symlink"));
        std::fs::remove_file(&alias).unwrap();

        std::fs::hard_link(&destination, &alias).unwrap();
        assert!(publish_export(&alias, &source, |_| Ok(()))
            .unwrap_err()
            .contains("hard links"));
        std::fs::remove_file(&alias).unwrap();

        let dest_race = destination.clone();
        assert!(publish_export(&destination, &source, move |file| {
            std::fs::write(&dest_race, "concurrent writer").unwrap();
            file.write_all(b"complete export").map_err(|e| e.to_string())
        })
        .unwrap_err()
        .contains("changed during export"));
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "concurrent writer");

        let dest_rm = destination.clone();
        assert!(publish_export(&destination, &source, move |file| {
            std::fs::remove_file(&dest_rm).unwrap();
            file.write_all(b"complete export").map_err(|e| e.to_string())
        })
        .unwrap_err()
        .contains("changed during export"));
        assert!(!destination.exists(), "removed destination is not recreated on race");
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "all failed publication stages removed"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
