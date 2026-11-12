//! clustervision desktop app — the Tauri v2 runtime.
//!
//! This crate is a window around the *existing* static web UI (`<repo>/web/`, wired via
//! `frontendDist` in `tauri.conf.json`). It adds the things the browser-only app can't do:
//!
//!   1. **One native command, `api(path, query)`**, which calls `cv_core::api::route_json` —
//!      the same function `cv serve` answers `/api/*` with — in-process. No HTTP (the webview's
//!      cross-origin fetch to `http://localhost` is unreliable) and no second implementation: a
//!      session cannot look different depending on which door it came through.
//!   2. **Launches `cv serve --port 7777` on startup** so the bundled fleet dashboard (which
//!      fetches `http://localhost:7777`) works unchanged. The child is killed on exit.
//!   3. A **native application menu** (File / Edit / View / Help) — including **File → Open zip…**
//!      which runs a native file dialog, ingests the zip in Rust (no WASM module), and emits the
//!      `cv://open-sessions` Tauri event the web UI listens for.
//!   4. **Window state persistence** (size/position across launches) via
//!      `tauri-plugin-window-state`.
//!
//! ### cv-serve wiring approach
//! We spawn `cv` via `std::process::Command` (rather than a bundled Tauri *sidecar*) — it's
//! the simplest thing that compiles and runs everywhere without an extra bundling step. We
//! search the repo's build outputs `target/{release,debug}/cv` first, then PATH, so a plain
//! `cargo build -p clustervision` in the repo is enough to make it work in dev. The handle is stored in
//! Tauri's managed state and killed on `RunEvent::Exit`. A **View → cv serve** menu toggle lets
//! you start/stop it at runtime.
//!
//! To ship a self-contained bundle later, build `cv` (`cargo build -p clustervision --release`) and
//! either drop the binary on PATH or register it as a Tauri sidecar (see `app/README.md`).

use std::io::Read;
use std::process::{Child, Command};
use std::sync::Mutex;

use tauri::menu::{AboutMetadataBuilder, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, RunEvent, Runtime, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

/// The port the bundled web fleet-dashboard expects `cv serve` on.
const SERVE_PORT: u16 = 7777;

/// The Tauri event the web UI listens for to load freshly-ingested sessions. Payload is a JSON
/// string holding a `Session[]` array (the same IR the WASM `ingest_zip` returns).
const OPEN_SESSIONS_EVENT: &str = "cv://open-sessions";

/// The public web demo, opened from Help → Open the web demo.
const WEB_DEMO_URL: &str = "https://emberian.github.io/clustervision/";

/// Handle to the spawned `cv serve` child, kept in Tauri managed state so we can kill it on exit
/// (and toggle it from the menu).
#[derive(Default)]
struct ServeChild(Mutex<Option<Child>>);

// ---------------------------------------------------------------------------
// Zip ingest (File → Open zip…)
// ---------------------------------------------------------------------------

/// Unzip `bytes` in-memory and ingest the contained files into `Session[]` JSON — the shape the
/// web UI's WASM `ingest_zip` produces, via the same `cv_core::ingest::ingest_files`.
fn ingest_zip_bytes(bytes: &[u8]) -> Result<String, String> {
    let files = cv_core::ingest::archive::extract(std::io::Cursor::new(bytes), Default::default())
        .map_err(|error| format!("zip: {error:#}"))?;
    let sessions = cv_core::ingest::ingest_files(files).map_err(|error| format!("ingesting zip: {error:#}"))?;
    serde_json::to_string(&sessions).map_err(|e| format!("serializing sessions: {e}"))
}

// ---------------------------------------------------------------------------
// Reading the machine's real sessions — one command over the one read API.
// ---------------------------------------------------------------------------

/// `GET <path>?<query>` against [`cv_core::api::route_json`], in-process: `path` is a read-API
/// path (`/api/sessions`, `/api/session/claude/<id>/messages`, …), `query` its raw query string.
/// A 2xx returns the JSON body as a string; anything else rejects with `"<status>: <error>"`.
#[tauri::command]
fn api(path: String, query: Option<String>) -> Result<String, String> {
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
    let (status, body) = cv_core::api::route_json(&parts, query.as_deref().unwrap_or(""));
    if (200..300).contains(&status) {
        serde_json::to_string(&body).map_err(|e| format!("serializing {path}: {e}"))
    } else {
        Err(format!(
            "{status}: {}",
            body["error"].as_str().unwrap_or("request failed")
        ))
    }
}

// Full transcript exports stay out of the webview: the existing checked CLI exporter
// writes a private file beside the destination, published only after successful exit.
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

fn export_to_path(
    binary: &std::path::Path,
    harness: &str,
    id: &str,
    format: &str,
    destination: &std::path::Path,
    source: &std::path::Path,
) -> Result<(), String> {
    let cli_format = match format {
        "markdown" => "md",
        "opensession" => "json",
        _ => return Err("unknown export format".into()),
    };
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
    let stage = ExportStage::create(&parent)?;
    let mut errors = ExportStage::create(&parent)?;
    let status = Command::new(binary)
        .args(["export", "--harness", harness, "--format", cli_format, "--", id])
        .stdin(std::process::Stdio::null())
        .stdout(stage.file.try_clone().map_err(|e| e.to_string())?)
        .stderr(errors.file.try_clone().map_err(|e| e.to_string())?)
        .status()
        .map_err(|e| format!("starting cv export: {e}"))?;
    if !status.success() {
        use std::io::{Seek, SeekFrom};
        errors.file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let mut message = String::new();
        (&mut errors.file)
            .take(4096)
            .read_to_string(&mut message)
            .map_err(|e| e.to_string())?;
        return Err(format!("cv export failed ({status}): {}", message.trim()));
    }
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
        let kind = cv_core::Harness::parse(&harness).ok_or("unknown source harness")?;
        let (reference, _) = cv_core::find(&id, Some(kind))
            .map_err(|e| format!("finding transcript source: {e}"))?
            .ok_or("transcript source not found")?;
        let primary = cv_core::source::primary_path(&reference);
        export_to_path(&cv_binary(), &harness, &id, &format, &destination, &primary)?;
        Ok(true)
    })
    .await
    .map_err(|e| format!("export task failed: {e}"))?
}

// ---------------------------------------------------------------------------
// Native application menu
// ---------------------------------------------------------------------------

/// Build the native application menu (File / View / Help). Menu item IDs are matched in
/// [`handle_menu_event`].
fn build_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    // --- File
    let open_zip = MenuItem::with_id(app, "file.open_zip", "Open zip…", true, Some("CmdOrCtrl+O"))?;
    let quit = PredefinedMenuItem::quit(app, Some("Quit clustervision"))?;
    let file = Submenu::with_items(app, "File", true, &[&open_zip, &quit])?;

    // --- Edit: without these, macOS gives the webview no Cmd+C/Cmd+V/Cmd+A — a real problem for a
    // transcript viewer where copying text is the whole point. Predefined items wire to the OS's
    // standard editing actions for the focused field/selection.
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
    let serve_toggle = MenuItem::with_id(app, "view.serve_toggle", "Toggle cv serve", true, None::<&str>)?;
    let view = Submenu::with_items(
        app,
        "View",
        true,
        &[
            &reload,
            &devtools,
            &fullscreen,
            &PredefinedMenuItem::separator(app)?,
            &serve_toggle,
        ],
    )?;

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
                // Reloading the webview re-fetches frontendDist (web/index.html).
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
        "view.serve_toggle" => toggle_serve(app),
        "help.web_demo" => {
            // Open in the user's default browser via the shell plugin (matches the existing
            // `shell:allow-open` capability). `Shell::open` is deprecated in favor of
            // tauri-plugin-opener, but using it avoids pulling in another plugin/permission set.
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

/// File → Open zip…: native picker → in-memory ingest → emit `cv://open-sessions` with the
/// `Session[]` JSON payload. All failures are surfaced via a native message dialog rather than
/// silently dropped.
fn open_zip_via_dialog<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    // Run the (blocking) native picker off the main thread so we never deadlock the event loop.
    std::thread::spawn(move || {
        let picked = app
            .dialog()
            .file()
            .add_filter("Harness export (.zip)", &["zip"])
            .set_title("Open a harness export zip")
            .blocking_pick_file();

        let Some(file_path) = picked else {
            return; // user cancelled
        };

        let result = file_path
            .into_path()
            .map_err(|e| format!("resolving picked path: {e}"))
            .and_then(|p| std::fs::read(&p).map_err(|e| format!("reading {}: {e}", p.display())))
            .and_then(|bytes| ingest_zip_bytes(&bytes));

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

// ---------------------------------------------------------------------------
// cv serve lifecycle
// ---------------------------------------------------------------------------

/// Locate the `cv` binary: the repo's build outputs relative to this crate's manifest dir first
/// (so a dev `cargo build -p clustervision` works without installing anything), then `cv` on PATH.
fn cv_binary() -> std::path::PathBuf {
    let name = if cfg!(windows) { "cv.exe" } else { "cv" };
    // CARGO_MANIFEST_DIR is `.../clustervision/app/src-tauri`; the workspace target dir is
    // `.../clustervision/target`.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = manifest.parent().and_then(|p| p.parent()) {
        for profile in ["release", "debug"] {
            let cand = root.join("target").join(profile).join(name);
            if cand.exists() {
                return cand;
            }
        }
    }
    std::path::PathBuf::from(name)
}

/// Spawn `cv serve --port 7777`. Errors are logged but non-fatal: the rest of the UI (everything
/// except the live fleet dashboard) still works without it. Returns the child on success.
fn spawn_serve() -> Option<Child> {
    let bin = cv_binary();
    match Command::new(&bin)
        .arg("serve")
        .arg("--port")
        .arg(SERVE_PORT.to_string())
        .spawn()
    {
        Ok(child) => {
            eprintln!(
                "clustervision: launched `{} serve --port {SERVE_PORT}` (pid {})",
                bin.display(),
                child.id()
            );
            Some(child)
        }
        Err(e) => {
            eprintln!(
                "clustervision: could not launch cv serve ({}): {e}. The fleet dashboard at \
                 http://localhost:{SERVE_PORT} will be unavailable (non-fatal). Build it with \
                 `cargo build -p clustervision --release` or put `cv` on PATH.",
                bin.display()
            );
            None
        }
    }
}

/// View → Toggle cv serve: kill it if running, otherwise (re)spawn it.
fn toggle_serve<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<ServeChild>();
    let mut guard = state.0.lock().unwrap();
    if let Some(mut child) = guard.take() {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("clustervision: stopped cv serve via menu toggle.");
    } else {
        *guard = spawn_serve();
    }
}

/// Build and run the Tauri application. Called by `main.rs`.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        // Persist each window's size/position/etc. across launches and restore on startup.
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .manage(ServeChild::default())
        .invoke_handler(tauri::generate_handler![api, export_session])
        .on_menu_event(handle_menu_event)
        .setup(|app| {
            // Install the native application menu.
            let menu = build_menu(app.handle())?;
            app.set_menu(menu)?;

            // Launch the fleet-state API so the bundled dashboard works unchanged.
            if let Some(child) = spawn_serve() {
                let state = app.state::<ServeChild>();
                *state.0.lock().unwrap() = Some(child);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building clustervision")
        .run(|app_handle, event| {
            // On exit, reap the cv serve child so we don't leave an orphaned server on :7777.
            if let RunEvent::Exit = event {
                let state = app_handle.state::<ServeChild>();
                let child = state.0.lock().unwrap().take();
                if let Some(mut child) = child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        });
}

// ---------------------------------------------------------------------------
// Tests — cover the native logic (zip ingest, the api command, binary lookup). The GUI shell
// (menu, window, event loop) can't be driven headlessly, but these are the parts with real bug
// surface, and they run without a webview via plain `cargo test`.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::Session;
    use std::io::Write as _;

    /// Build an in-memory zip from `(name, bytes)` entries using `Stored` (no compression feature
    /// needed), returning the raw zip bytes.
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    /// A minimal Claude `.jsonl` transcript: ingest sniffs it by `sessionId` + `type:user|assistant`.
    const CLAUDE_JSONL: &str = concat!(
        r#"{"type":"user","sessionId":"sess-abc","parentUuid":null,"message":{"role":"user","content":"hello"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"sess-abc","parentUuid":null,"message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#,
        "\n",
    );

    #[test]
    fn ingest_zip_bytes_parses_a_claude_export() {
        let zip = make_zip(&[("logs/session.jsonl", CLAUDE_JSONL.as_bytes())]);
        let json = ingest_zip_bytes(&zip).expect("ingest should succeed");
        let sessions: Vec<Session> = serde_json::from_str(&json).expect("valid Session[] JSON");
        assert_eq!(sessions.len(), 1, "one transcript → one session");
        assert_eq!(sessions[0].harness, cv_core::Harness::Claude);
        assert!(
            !sessions[0].messages.is_empty(),
            "the session should carry its messages"
        );
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
        // A directory entry and an unrecognized file should yield an empty (but valid) array, no err.
        let zip = make_zip(&[("notes/", b""), ("random.txt", b"just some prose, not a transcript")]);
        let json = ingest_zip_bytes(&zip).expect("ingest should not error on unknown content");
        let sessions: Vec<Session> = serde_json::from_str(&json).unwrap();
        assert!(sessions.is_empty(), "nothing recognizable → []");
    }

    #[test]
    fn api_command_routes_onto_the_read_api() {
        // The command is `route_json` behind a path splitter: a known route answers and an unknown
        // or retired route rejects with 404.
        let health: serde_json::Value = serde_json::from_str(&api("/api/health".into(), None).unwrap()).unwrap();
        assert_eq!(health["ok"], true);
        assert!(api("/api/nope".into(), None).unwrap_err().starts_with("404"));
        assert!(api("/api/channels".into(), None).unwrap_err().starts_with("404"));
        assert!(api("/api/sessions".into(), Some("harness=warpdrive".into()))
            .unwrap_err()
            .starts_with("400"));
    }

    #[cfg(unix)]
    #[test]
    fn export_publication_preserves_destination_on_failure() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "cv-export-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let script = root.join("cv-test");
        let destination = root.join("session.json");
        std::fs::write(&destination, "previous complete export").unwrap();
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf partial\nprintf 'broken source' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = export_to_path(&script, "claude", "session", "opensession", &destination, &script).unwrap_err();
        assert!(error.contains("broken source"));
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "previous complete export"
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2, "private stages removed");
        std::fs::write(&script, "#!/bin/sh\nprintf 'complete export'\n").unwrap();
        export_to_path(&script, "claude", "session", "opensession", &destination, &script).unwrap();
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "complete export");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2, "success stages removed");
        assert_eq!(
            std::fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            export_to_path(&script, "claude", "session", "opensession", &script, &script)
                .unwrap_err()
                .contains("transcript source")
        );
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&destination, &alias).unwrap();
        assert!(
            export_to_path(&script, "claude", "session", "opensession", &alias, &script)
                .unwrap_err()
                .contains("symlink")
        );
        std::fs::remove_file(&alias).unwrap();
        std::fs::hard_link(&destination, &alias).unwrap();
        assert!(
            export_to_path(&script, "claude", "session", "opensession", &alias, &script)
                .unwrap_err()
                .contains("hard links")
        );
        std::fs::remove_file(&alias).unwrap();
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'concurrent writer' > \"${0%/*}/session.json\"\nprintf 'complete export'\n",
        )
        .unwrap();
        assert!(
            export_to_path(&script, "claude", "session", "opensession", &destination, &script)
                .unwrap_err()
                .contains("changed during export")
        );
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "concurrent writer");
        std::fs::remove_file(&destination).unwrap();
        assert!(
            export_to_path(&script, "claude", "session", "opensession", &destination, &script)
                .unwrap_err()
                .contains("changed during export")
        );
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "concurrent writer");
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            2,
            "all failed publication stages removed"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cv_binary_resolves_to_a_cv_path() {
        // Either a built binary under target/{release,debug}/cv, or the bare PATH fallback — but it
        // must always end in the executable name so the spawn targets the right thing.
        let p = cv_binary();
        let name = if cfg!(windows) { "cv.exe" } else { "cv" };
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some(name));
    }
}
