//! The pieces of `cv share`'s self-contained HTML artifact: `share_begin` / `share_message` /
//! `share_end`, driven by the CLI's streaming sink so a multi-GB transcript renders at
//! O(largest message). One `<!doctype html>` document, CSS and the small nav script inlined, no
//! external assets; it opens offline by double-click and follows `prefers-color-scheme`. Every
//! session-derived string is HTML-escaped (see [`escape`]) so a transcript containing `<script>`
//! etc. can never inject markup into the page.

use crate::ir::{human_bytes, Block, Harness, Message, Role, Usage};
#[cfg(test)]
use crate::json::{ReadInput, Validation};

fn render_usage(u: &Usage, out: &mut String) {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = u.input_tokens {
        parts.push(format!("in {v}"));
    }
    if let Some(v) = u.output_tokens {
        parts.push(format!("out {v}"));
    }
    if let Some(v) = u.cache_read_tokens {
        parts.push(format!("cache read {v}"));
    }
    if let Some(v) = u.cache_creation_tokens {
        parts.push(format!("cache write {v}"));
    }
    if parts.is_empty() {
        return;
    }
    out.push_str("<div class=\"usage\">tokens: ");
    out.push_str(&escape(&parts.join(" · ")));
    out.push_str("</div>\n");
}

/// Render plain text with minimal, *safe* markdown niceties:
/// - blank lines split paragraphs (`<p>`),
/// - single newlines become `<br>`,
/// - triple-backtick fenced blocks become `<pre><code>`,
/// - inline `` `code` `` becomes `<code>`.
///
/// All text is escaped first; we never trust session content as markup.
fn render_text(text: &str, out: &mut String) {
    // Split on fenced code blocks. A fence is a line that is exactly ``` (optionally with a lang).
    let mut in_fence = false;
    let mut fence_buf = String::new();
    let mut para_buf = String::new();

    let flush_para = |para: &mut String, out: &mut String| {
        let trimmed = para.trim_matches('\n');
        if trimmed.is_empty() {
            para.clear();
            return;
        }
        out.push_str("<p>");
        render_inline(trimmed, out);
        out.push_str("</p>\n");
        para.clear();
    };

    for line in text.split('\n') {
        let is_fence = line.trim_start().starts_with("```");
        if is_fence {
            if in_fence {
                // close fence
                out.push_str("<pre><code>");
                out.push_str(&escape(&fence_buf));
                out.push_str("</code></pre>\n");
                fence_buf.clear();
                in_fence = false;
            } else {
                // open fence — flush any pending paragraph first
                flush_para(&mut para_buf, out);
                in_fence = true;
            }
            continue;
        }
        if in_fence {
            fence_buf.push_str(line);
            fence_buf.push('\n');
            continue;
        }
        if line.trim().is_empty() {
            flush_para(&mut para_buf, out);
        } else {
            if !para_buf.is_empty() {
                para_buf.push('\n');
            }
            para_buf.push_str(line);
        }
    }
    // unterminated fence: render what we have as a code block.
    if in_fence && !fence_buf.is_empty() {
        out.push_str("<pre><code>");
        out.push_str(&escape(&fence_buf));
        out.push_str("</code></pre>\n");
    }
    flush_para(&mut para_buf, out);
}

/// Render a single paragraph's worth of text: escape everything, turn single newlines into
/// `<br>`, and wrap inline `` `code` `` spans in `<code>`.
fn render_inline(s: &str, out: &mut String) {
    let mut rest = s;
    while let Some(start) = rest.find('`') {
        // text before the backtick
        push_with_breaks(&rest[..start], out);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('`') {
            out.push_str("<code>");
            out.push_str(&escape(&after[..end]));
            out.push_str("</code>");
            rest = &after[end + 1..];
        } else {
            // unmatched backtick: treat literally
            push_with_breaks(&rest[start..], out);
            return;
        }
    }
    push_with_breaks(rest, out);
}

/// Escape `s` and convert single `\n` into `<br>`.
fn push_with_breaks(s: &str, out: &mut String) {
    let mut first = true;
    for line in s.split('\n') {
        if !first {
            out.push_str("<br>\n");
        }
        first = false;
        out.push_str(&escape(line));
    }
}

/// HTML-escape the five significant characters. Hand-rolled to keep cv-core dep-light.
/// This is the single trust boundary: every session-derived string passes through here.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn role_class(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// A stable color for each harness badge; the five without one of their own share the neutral.
fn harness_color(h: Harness) -> &'static str {
    match h {
        Harness::Claude => "#d97757",
        Harness::Codex => "#10a37f",
        Harness::Grok => "#1d1d1f",
        Harness::OpenCode => "#5b6ee1",
        Harness::Gemini => "#4285f4",
        Harness::Antigravity => "#1a73e8",
        Harness::Hermes => "#8e44ad",
        Harness::OpenClaw => "#b8860b",
        Harness::Cursor => "#111111",
        Harness::Kimi => "#0050ff",
        Harness::Qwen => "#6a3ec0",
        Harness::LmStudio => "#2563eb",
        Harness::Cline => "#0aa",
        Harness::Roo => "#e0533d",
        Harness::Continue => "#444",
        Harness::Goose => "#7a5c3e",
        Harness::Devin => "#3b6ee8",
        Harness::KimiCode | Harness::Zed | Harness::ChatGptExport | Harness::ClaudeExport | Harness::OpenSession => {
            "#555"
        }
    }
}

/// Session-level facts the share artifact's header and footer need. Everything here is known
/// before (or shortly after) the body starts streaming, so the caller can hold back only a small
/// message buffer while it fills in `label`/`model`.
pub struct ShareMeta {
    /// Human label (title or first user text), already redacted by the caller when applicable.
    pub label: String,
    pub harness: crate::ir::Harness,
    pub id: String,
    pub model: Option<String>,
    /// Working directory, pre-shortened by the caller (e.g. `~/proj`) so it leaks no username.
    pub cwd: Option<String>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether the redaction pass ran (drives the header/footer badges).
    pub redacted: bool,
}

/// Everything up to and including `<main>`: doctype, CSP, inline CSS, sticky header.
pub fn share_begin(meta: &ShareMeta) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(SHARE_CSS.len() + 2048);
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    // Belt-and-braces: even if an escaping bug ever slipped markup through, the document may not
    // load anything remote. Inline style/script only, data: images only.
    out.push_str(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:\">\n",
    );
    out.push_str("<title>");
    out.push_str(&escape(&meta.label));
    out.push_str(" · clustervision</title>\n<style>\n");
    out.push_str(SHARE_CSS);
    out.push_str("</style>\n</head>\n<body>\n");

    out.push_str("<header>\n<div class=\"bar\">\n<span class=\"orb\" aria-hidden=\"true\">🔮</span>\n");
    out.push_str("<div class=\"head-main\">\n<h1>");
    out.push_str(&escape(&meta.label));
    out.push_str("</h1>\n<div class=\"sub\">");
    let _ = write!(
        out,
        "<span class=\"badge\" style=\"background:{}\">{}</span>",
        harness_color(meta.harness),
        escape(meta.harness.as_str()),
    );
    if meta.redacted {
        out.push_str(
            "<span class=\"badge shield\" title=\"secrets/PII were scrubbed before export\">🛡 redacted</span>",
        );
    } else {
        out.push_str("<span class=\"badge warn\" title=\"exported with --no-redact\">⚠ unredacted</span>");
    }
    if let Some(model) = &meta.model {
        let _ = write!(out, "<span class=\"chip\">{}</span>", escape(model));
    }
    if let Some(t) = &meta.created_at {
        let _ = write!(out, "<span class=\"chip\">{}</span>", t.format("%Y-%m-%d"));
    }
    if let Some(cwd) = &meta.cwd {
        let _ = write!(out, "<span class=\"chip\">{}</span>", escape(cwd));
    }
    let _ = write!(
        out,
        "<span class=\"chip\" title=\"session id\">{}</span>",
        escape(&short(&meta.id))
    );
    // Filled in by the inline script (the count isn't known when this header streams out).
    out.push_str("<span class=\"chip\" id=\"cv-count\"></span>");
    out.push_str("</div>\n</div>\n");
    out.push_str("<button id=\"cv-expand\" type=\"button\" title=\"open/close every tool call and thinking block (keyboard: j/k to move, x to toggle)\">expand all</button>\n");
    out.push_str("</div>\n</header>\n<main>\n");
    out
}

/// One message card. `idx` becomes the anchor id (`#m0`, `#m1`, …) used by keyboard nav.
/// A source error may leave a prefix in `out`; callers must discard the staged card on failure.
/// Render lazy text using the caller's session source for implicit spans. Failed source reads
/// leave only a staged prefix, which the caller must discard.
pub fn share_message(m: &Message, idx: usize, out: &mut String, resolver: &crate::Resolver) -> anyhow::Result<()> {
    use std::fmt::Write as _;
    let role = role_class(m.role);
    let _ = writeln!(out, "<section class=\"msg msg-{role}\" id=\"m{idx}\">");
    out.push_str("<div class=\"msg-head\"><span class=\"role\">");
    out.push_str(&escape(&crate::render::turn_label(m)));
    out.push_str("</span>");
    if let Some(t) = &m.timestamp {
        let _ = write!(out, "<span class=\"ts\">{}</span>", t.format("%Y-%m-%d %H:%M UTC"));
    }
    out.push_str("</div>\n<div class=\"msg-body\">\n");
    for b in &m.content {
        share_block(b, out, resolver)?;
    }
    out.push_str("</div>\n");
    if let Some(u) = &m.usage {
        render_usage(u, out);
    }
    out.push_str("</section>\n");
    Ok(())
}

fn share_block(b: &Block, out: &mut String, resolver: &crate::Resolver) -> anyhow::Result<()> {
    use std::fmt::Write as _;
    match b {
        Block::Text { text } => {
            let text = text.try_resolve(resolver)?;
            out.push_str("<div class=\"text\">");
            render_text(&text, out);
            out.push_str("</div>\n");
        }
        Block::Thinking { text, redacted, .. } => {
            let text = text.try_resolve(resolver)?;
            out.push_str("<details class=\"fold think\"><summary><span class=\"t-ico\">🧠</span> thinking");
            if *redacted {
                out.push_str(" <em>(redacted by provider)</em>");
            }
            let _ = write!(
                out,
                "<span class=\"hint\">{}</span>",
                escape(&crate::ir::truncate(&text, 72))
            );
            out.push_str("</summary>\n");
            if *redacted && text.trim().is_empty() {
                out.push_str("<p class=\"redacted\">[redacted by provider]</p>");
            } else {
                out.push_str("<div class=\"text\">");
                render_text(&text, out);
                out.push_str("</div>");
            }
            out.push_str("\n</details>\n");
        }
        Block::ToolUse { name, input, .. } => {
            let preview = input_preview(input)?;
            let input = input.resolve()?;
            out.push_str("<details class=\"fold tool tool-use\"><summary><span class=\"t-ico\">🔧</span> <code>");
            out.push_str(&escape(name));
            out.push_str("</code>");
            let _ = write!(out, "<span class=\"hint\">{}</span>", escape(&preview));
            out.push_str("</summary>\n<pre><code>");
            let pretty = serde_json::to_string_pretty(&*input)?;
            out.push_str(&escape(&pretty));
            out.push_str("</code></pre>\n</details>\n");
        }
        Block::ToolResult {
            content,
            is_error,
            tool_name,
            status,
            ..
        } => {
            let content = content.try_resolve(resolver)?;
            let cls = if *is_error {
                "fold tool tool-res err"
            } else {
                "fold tool tool-res"
            };
            let _ = write!(
                out,
                "<details class=\"{cls}\"><summary><span class=\"t-ico\">↩</span> result"
            );
            if let Some(name) = tool_name {
                out.push_str(" <code>");
                out.push_str(&escape(name));
                out.push_str("</code>");
            }
            if *is_error {
                out.push_str(" <span class=\"err-tag\">error</span>");
            } else if let Some(s) = status {
                let _ = write!(out, " <span class=\"status\">{}</span>", escape(s));
            }
            let _ = write!(out, "<span class=\"hint\">{}</span>", human_bytes(content.len() as u64));
            out.push_str("</summary>\n<pre><code>");
            out.push_str(&escape(&content));
            out.push_str("</code></pre>\n</details>\n");
        }
        Block::File { path, source, .. } => {
            let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
            out.push_str("<div class=\"attach\">📎 file: <code>");
            out.push_str(&escape(label));
            out.push_str("</code></div>\n");
        }
        Block::Image { media_type, .. } => {
            out.push_str("<div class=\"attach\">🖼 [image]");
            if let Some(mt) = media_type {
                out.push_str(" <code>");
                out.push_str(&escape(mt));
                out.push_str("</code>");
            }
            out.push_str("</div>\n");
        }
    }
    Ok(())
}

/// Footer + inline script + closing tags. `redactions` is the number of spans the scrubber
/// replaced (shown when `meta.redacted`); `version` is the emitting `cv`'s crate version.
pub fn share_end(meta: &ShareMeta, redactions: usize, version: &str) -> String {
    let mut out = String::with_capacity(SHARE_JS.len() + 512);
    out.push_str("</main>\n<footer>\n<div class=\"foot-line\">🔮 shared with <strong>clustervision</strong> v");
    out.push_str(&escape(version));
    out.push_str("</div>\n");
    if meta.redacted {
        out.push_str("<div class=\"foot-badge shield\">🛡 this transcript was redacted — ");
        if redactions > 0 {
            out.push_str(&format!(
                "{redactions} secret{} scrubbed",
                if redactions == 1 { "" } else { "s" }
            ));
        } else {
            out.push_str("nothing sensitive found");
        }
        out.push_str("</div>\n");
    } else {
        out.push_str("<div class=\"foot-badge warn\">⚠ shared without redaction</div>\n");
    }
    out.push_str("</footer>\n<script>\n");
    out.push_str(SHARE_JS);
    out.push_str("</script>\n</body>\n</html>\n");
    out
}

/// First 8 chars of an id for the header chip.
fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A one-line preview of a tool input for the collapsed summary: the most human-meaningful
/// string field if present, otherwise the compact JSON.
fn input_preview(input: &crate::Json) -> anyhow::Result<String> {
    const KEYS: &[&str] = &[
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "prompt",
        "description",
    ];
    // One extra scalar determines truncation; the shared projection stops decoding the tail.
    if let Some(preview) = input.with_view(|view| {
        for key in KEYS {
            if let Some(field) = view.get(key)? {
                if let Some(head) = crate::projection::Head::of_string(field, 80)? {
                    return Ok(Some(head));
                }
            }
        }
        Ok(None)
    })? {
        return Ok(preview);
    }
    input.with_view(|view| crate::projection::Head::of_json(view, 80))
}

/// The 🔮 share theme: dark by default (deep violet-black with a soft crystal glow), light via
/// `prefers-color-scheme`. All selectors are namespaced by document structure — this CSS only ever
/// lives in its own document.
const SHARE_CSS: &str = r##":root {
  color-scheme: dark;
  --bg: #0f0d17;
  --fg: #eae6f5;
  --muted: #9c94b8;
  --card: #161224;
  --card-border: #2b2342;
  --head-bg: rgba(17, 13, 28, .88);
  --accent: #a78bfa;
  --user: #7dd3fc;
  --assistant: #a78bfa;
  --system: #f0abfc;
  --tool: #e3b364;
  --code-bg: #0c0a14;
  --inline-code-bg: #241c3c;
  --error-bg: #2c1520;
  --error-fg: #f38ba8;
  --shield: #34d399;
  --warn: #fbbf24;
  --hl-kw: #c099ff;
  --hl-str: #9ece8f;
  --hl-num: #e5b566;
  --hl-com: #6f6890;
}
@media (prefers-color-scheme: light) {
  :root {
    color-scheme: light;
    --bg: #faf9fd;
    --fg: #241f33;
    --muted: #6c6685;
    --card: #ffffff;
    --card-border: #e4e0f0;
    --head-bg: rgba(250, 249, 253, .92);
    --user: #0369a1;
    --assistant: #7c3aed;
    --system: #be185d;
    --tool: #b45309;
    --code-bg: #f4f2fa;
    --inline-code-bg: #ece8f8;
    --error-bg: #fdecf0;
    --error-fg: #be123c;
    --shield: #047857;
    --warn: #b45309;
    --hl-kw: #7c3aed;
    --hl-str: #15803d;
    --hl-num: #b45309;
    --hl-com: #8b86a0;
  }
}
* { box-sizing: border-box; }
html { scroll-behavior: smooth; }
body {
  margin: 0;
  background: var(--bg);
  background-image: radial-gradient(900px 420px at 50% -120px, rgba(139, 92, 246, .18), transparent 70%);
  background-repeat: no-repeat;
  color: var(--fg);
  font: 15px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
}
header {
  position: sticky; top: 0; z-index: 10;
  background: var(--head-bg);
  backdrop-filter: blur(10px); -webkit-backdrop-filter: blur(10px);
  border-bottom: 1px solid var(--card-border);
}
.bar { max-width: 880px; margin: 0 auto; display: flex; align-items: center; gap: .75rem; padding: .55rem 1rem; }
.orb { font-size: 1.45rem; filter: drop-shadow(0 0 8px rgba(167, 139, 250, .55)); }
.head-main { flex: 1; min-width: 0; }
h1 { font-size: 1rem; margin: 0; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.sub { display: flex; flex-wrap: wrap; gap: .4rem; align-items: center; font-size: .72rem; color: var(--muted); margin-top: .2rem; }
.badge {
  display: inline-block; color: #fff; font-weight: 600; text-transform: uppercase;
  letter-spacing: .05em; font-size: .64rem; padding: .08rem .5rem; border-radius: 999px;
}
.badge.shield { background: transparent; color: var(--shield); border: 1px solid var(--shield); }
.badge.warn { background: transparent; color: var(--warn); border: 1px solid var(--warn); }
.chip {
  background: var(--inline-code-bg); border-radius: 4px; padding: .05rem .4rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: .68rem;
  max-width: 22ch; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
#cv-expand {
  background: none; border: 1px solid var(--card-border); color: var(--muted);
  border-radius: 6px; padding: .25rem .6rem; font-size: .72rem; cursor: pointer; flex-shrink: 0;
}
#cv-expand:hover { color: var(--fg); border-color: var(--accent); }
main { max-width: 880px; margin: 1.4rem auto 0; padding: 0 1rem; }
.msg {
  border: 1px solid var(--card-border); border-left-width: 3px; border-radius: 10px;
  margin: 0 0 .9rem; background: var(--card); overflow: hidden; scroll-margin-top: 4.6rem;
}
.msg.focus { box-shadow: 0 0 0 2px var(--accent); }
.msg-user { border-left-color: var(--user); }
.msg-assistant { border-left-color: var(--assistant); }
.msg-system { border-left-color: var(--system); }
.msg-tool { border-left-color: var(--tool); }
.msg-head {
  display: flex; justify-content: space-between; align-items: baseline;
  padding: .45rem .9rem; background: rgba(127, 127, 170, .06);
  border-bottom: 1px solid var(--card-border);
}
.role { font-weight: 700; text-transform: uppercase; font-size: .72rem; letter-spacing: .06em; }
.msg-user .role { color: var(--user); }
.msg-assistant .role { color: var(--assistant); }
.msg-system .role { color: var(--system); }
.msg-tool .role { color: var(--tool); }
.ts { font-size: .7rem; color: var(--muted); font-variant-numeric: tabular-nums; }
.msg-body { padding: .25rem .9rem .6rem; }
.text p { margin: .6rem 0; word-wrap: break-word; overflow-wrap: anywhere; }
pre {
  background: var(--code-bg); border: 1px solid var(--card-border); border-radius: 8px;
  padding: .7rem .8rem; overflow-x: auto; margin: .5rem 0;
  font: .8rem/1.55 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
code { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: .9em; }
.text code, summary code { background: var(--inline-code-bg); padding: .05rem .3rem; border-radius: 4px; }
pre code { background: none; padding: 0; }
details.fold {
  margin: .55rem 0; border: 1px solid var(--card-border); border-radius: 8px;
  background: rgba(127, 127, 170, .05); padding: 0;
}
details.fold > summary {
  cursor: pointer; padding: .4rem .7rem; font-size: .8rem; font-weight: 600;
  color: var(--muted); display: flex; align-items: baseline; gap: .35rem;
  list-style-position: inside; user-select: none;
}
details.fold > summary:hover { color: var(--fg); }
details.fold[open] > summary { border-bottom: 1px solid var(--card-border); }
details.fold > pre, details.fold > .text, details.fold > p { margin: .5rem .7rem; }
details.think > summary .t-ico { opacity: .8; }
details.tool-use > summary code { color: var(--tool); }
details.tool-res > summary code { color: var(--shield); }
details.tool-res.err > summary { color: var(--error-fg); }
details.tool-res.err pre { background: var(--error-bg); border-color: var(--error-fg); }
.hint {
  color: var(--muted); font-weight: 400; font-size: .74rem; margin-left: .4rem;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; min-width: 0;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
.err-tag { background: var(--error-fg); color: #fff; border-radius: 4px; padding: 0 .35rem; font-size: .68rem; }
.status { color: var(--muted); font-weight: 400; font-size: .74rem; }
.redacted { color: var(--muted); font-style: italic; }
.attach { font-size: .85rem; color: var(--muted); margin: .4rem 0; }
.usage {
  font-size: .7rem; color: var(--muted); padding: .3rem .9rem;
  border-top: 1px solid var(--card-border); background: rgba(127, 127, 170, .04);
  font-variant-numeric: tabular-nums;
}
.hl-kw { color: var(--hl-kw); }
.hl-str { color: var(--hl-str); }
.hl-num { color: var(--hl-num); }
.hl-com { color: var(--hl-com); font-style: italic; }
footer {
  max-width: 880px; margin: 2.2rem auto 0; padding: 1rem 1rem 2rem;
  border-top: 1px solid var(--card-border); text-align: center;
  font-size: .8rem; color: var(--muted);
}
.foot-badge { margin-top: .4rem; font-size: .74rem; }
.foot-badge.shield { color: var(--shield); }
.foot-badge.warn { color: var(--warn); }
@media (max-width: 640px) {
  .chip { max-width: 14ch; }
  #cv-expand { display: none; }
}
"##;

/// Inline behavior: message count, j/k keyboard nav, expand/collapse-all, and a tiny
/// regex-token highlighter. Operates only on `textContent` (never injects markup from content)
/// and degrades to nothing without JS — the document is fully readable static HTML.
const SHARE_JS: &str = r##"(function () {
  "use strict";
  var msgs = [].slice.call(document.querySelectorAll(".msg"));
  var count = document.getElementById("cv-count");
  if (count) count.textContent = msgs.length + " messages";

  // j/k navigation with a focus ring; x toggles the focused message's folds.
  var cur = -1;
  function go(d) {
    if (!msgs.length) return;
    cur = Math.max(0, Math.min(msgs.length - 1, cur + d));
    msgs.forEach(function (m) { m.classList.remove("focus"); });
    msgs[cur].classList.add("focus");
    msgs[cur].scrollIntoView({ block: "start" });
  }
  document.addEventListener("keydown", function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    var t = e.target;
    if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
    if (e.key === "j") { go(1); e.preventDefault(); }
    else if (e.key === "k") { go(-1); e.preventDefault(); }
    else if (e.key === "x" && cur >= 0) {
      var folds = msgs[cur].querySelectorAll("details");
      var open = ![].some.call(folds, function (d) { return d.open; });
      [].forEach.call(folds, function (d) { d.open = open; });
    }
  });

  var btn = document.getElementById("cv-expand");
  var allOpen = false;
  if (btn) btn.addEventListener("click", function () {
    allOpen = !allOpen;
    [].forEach.call(document.querySelectorAll("details"), function (d) { d.open = allOpen; });
    btn.textContent = allOpen ? "collapse all" : "expand all";
  });

  // Tiny highlighter: comments / strings / numbers / keywords, applied to code blocks under a
  // size cap. Built from text nodes only, so it can never introduce markup.
  var KW = "fn|let|mut|pub|use|impl|struct|enum|match|if|else|for|while|loop|return|async|await|" +
    "const|static|trait|mod|crate|self|where|move|ref|in|as|dyn|break|continue|function|var|class|" +
    "import|from|export|def|elif|except|try|catch|finally|with|lambda|None|True|False|null|true|" +
    "false|undefined|new|this|typeof|switch|case|default|do|then|fi|esac|done|local|echo|type";
  var TOK = new RegExp(
    "(\\/\\/[^\\n]*|#[^\\n]*|\\/\\*[\\s\\S]*?\\*\\/)" +
    "|(\"(?:[^\"\\\\\\n]|\\\\.)*\"|'(?:[^'\\\\\\n]|\\\\.)*')" +
    "|\\b(0x[0-9a-fA-F_]+|\\d[\\d_]*(?:\\.\\d+)?(?:[eE][+-]?\\d+)?)\\b" +
    "|\\b(" + KW + ")\\b", "g");
  function highlight(code) {
    var text = code.textContent;
    if (text.length > 30000) return;
    var frag = document.createDocumentFragment();
    var last = 0, m;
    TOK.lastIndex = 0;
    while ((m = TOK.exec(text))) {
      if (m.index > last) frag.appendChild(document.createTextNode(text.slice(last, m.index)));
      var span = document.createElement("span");
      span.className = m[1] ? "hl-com" : m[2] ? "hl-str" : m[3] ? "hl-num" : "hl-kw";
      span.textContent = m[0];
      frag.appendChild(span);
      last = m.index + m[0].length;
    }
    if (!last) return;
    frag.appendChild(document.createTextNode(text.slice(last)));
    code.textContent = "";
    code.appendChild(frag);
  }
  var blocks = [].slice.call(document.querySelectorAll("pre code"));
  function drain(deadline) {
    while (blocks.length && (!deadline || deadline.timeRemaining() > 2)) highlight(blocks.shift());
    if (blocks.length) schedule();
  }
  function schedule() {
    if (window.requestIdleCallback) requestIdleCallback(drain);
    else setTimeout(drain, 16);
  }
  schedule();
})();
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, GitInfo, Harness, Message, Role, Session, Usage};

    fn sample_session() -> Session {
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "Hello & welcome. Try <script>alert(1)</script> please.\n\nSecond para with `inline` code.".into(),
        });

        let mut assistant = Message::new(Role::Assistant);
        assistant.content.push(Block::Thinking {
            text: "I should be careful with <html>.".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        assistant.content.push(Block::Text {
            text: "Sure! Here is code:\n\n```rust\nfn main() { println!(\"<hi>\"); }\n```".into(),
        });
        assistant.content.push(Block::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: (serde_json::json!({ "command": "echo <x> & done" })).into(),
            namespace: None,
        });
        assistant.usage = Some(Usage {
            input_tokens: Some(42),
            output_tokens: Some(7),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            cost_usd: None,
        });

        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "t1".into(),
            content: "stdout: <x> & done".into(),
            is_error: true,
            tool_name: Some("Bash".into()),
            status: Some("error".into()),
            details: None,
        });
        tool.content.push(Block::File {
            mime: None,
            path: Some("/tmp/<weird>&.txt".into()),
            source: None,
        });
        tool.content.push(Block::Image {
            media_type: Some("image/png".into()),
            data_ref: None,
        });

        let mut system = Message::new(Role::System);
        system.content.push(Block::Text {
            text: "You are a helpful assistant.".into(),
        });

        Session {
            cwd: Some("/home/<user>/proj".into()),
            title: Some("Test & <Session>".into()),
            model: Some("claude-opus".into()),
            git: Some(GitInfo {
                branch: Some("dev".into()),
                commit: Some("deadbeef".into()),
                remote: None,
            }),
            messages: vec![system, user, assistant, tool],
            ..Session::new("abc123", Harness::Claude)
        }
    }

    #[test]
    fn escape_handles_all_specials() {
        assert_eq!(escape("a&b"), "a&amp;b");
        assert_eq!(escape("<x>"), "&lt;x&gt;");
        assert_eq!(escape("\"q\""), "&quot;q&quot;");
        assert_eq!(escape("'s'"), "&#39;s&#39;");
        assert_eq!(escape("plain"), "plain");
    }

    fn sample_share_meta(redacted: bool) -> ShareMeta {
        ShareMeta {
            label: "Test & <Session>".into(),
            harness: Harness::Claude,
            id: "abc123def456".into(),
            model: Some("claude-opus".into()),
            cwd: Some("~/proj".into()),
            created_at: None,
            redacted,
        }
    }

    fn full_share_doc(redacted: bool) -> String {
        let session = sample_session();
        let meta = sample_share_meta(redacted);
        let mut doc = share_begin(&meta);
        for (i, m) in session.messages.iter().enumerate() {
            share_message(m, i, &mut doc, &crate::Resolver::new(None)).unwrap();
        }
        doc.push_str(&share_end(&meta, 3, "0.9.11"));
        doc
    }

    #[test]
    fn share_doc_is_self_contained_and_escaped() {
        let html = full_share_doc(true);
        assert!(html.starts_with("<!doctype html"));
        assert!(html.trim_end().ends_with("</html>"));
        assert!(html.contains("<style>"));
        assert!(html.contains("Content-Security-Policy"));
        // no external loads, ever
        let lower = html.to_lowercase();
        assert!(!lower.contains("src=\"http"), "external src");
        assert!(!lower.contains("href=\"http"), "external href");
        assert!(!lower.contains("href='http"));
        assert!(!lower.contains("@import"));
        // label escaped in both <title> and <h1>
        assert!(html.contains("Test &amp; &lt;Session&gt;"));
        assert!(!html.contains("Test & <Session>"));
        // session content escaped (the XSS payload from sample_session)
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>alert"));
        // balanced message sections
        assert_eq!(html.matches("<section").count(), html.matches("</section>").count());
        assert_eq!(html.matches("<details").count(), html.matches("</details>").count());
    }

    #[test]
    fn share_collapsibles_and_summaries() {
        let html = full_share_doc(true);
        // thinking, tool use, and tool result are all <details> folds
        assert!(html.contains("details class=\"fold think\""), "thinking fold");
        assert!(html.contains("fold tool tool-use"), "tool-use fold");
        assert!(html.contains("fold tool tool-res err"), "error result fold");
        // tool-use summary previews the command, not the whole JSON
        assert!(html.contains("echo &lt;x&gt; &amp; done"));
        // anchors for keyboard nav
        assert!(html.contains("id=\"m0\""));
        assert!(html.contains("id=\"m3\""));
    }

    #[test]
    fn share_redaction_badges() {
        let on = full_share_doc(true);
        assert!(on.contains("🛡 redacted"), "header badge");
        assert!(on.contains("this transcript was redacted"), "footer badge");
        assert!(on.contains("3 secrets scrubbed"));
        assert!(!on.contains("unredacted"));

        let off = full_share_doc(false);
        assert!(off.contains("⚠ unredacted"), "header badge");
        assert!(off.contains("shared without redaction"), "footer badge");
        assert!(!off.contains("this transcript was redacted"));
    }

    #[test]
    fn share_footer_credits_version() {
        let html = full_share_doc(true);
        assert!(html.contains("clustervision</strong> v0.9.11"));
    }

    #[test]
    fn source_input_preview_matches_owned_and_stays_short() {
        let dir = std::env::temp_dir().join(format!("cv-html-json-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        for value in [
            serde_json::json!({"command": "é\n".repeat(10000), "payload": "ignored".repeat(10000)}),
            serde_json::json!({"opaque": "é\\\"".repeat(10000)}),
            serde_json::json!({"command": 4, "description": ""}),
            serde_json::json!([true, null, "wide α"]),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            let path = dir.join("input");
            std::fs::write(&path, &bytes).unwrap();
            let source = crate::Json::source(&path, 0, bytes.len() as u64).unwrap();
            let owned = crate::Json::Owned(value.clone());
            let expected = value
                .as_object()
                .and_then(|object| {
                    [
                        "command",
                        "file_path",
                        "path",
                        "pattern",
                        "query",
                        "url",
                        "prompt",
                        "description",
                    ]
                    .into_iter()
                    .find_map(|key| object.get(key).and_then(serde_json::Value::as_str))
                })
                .map(|text| crate::ir::truncate(text, 80))
                .unwrap_or_else(|| crate::ir::truncate(&value.to_string(), 80));
            assert_eq!(input_preview(&source).unwrap(), expected);
            assert_eq!(input_preview(&owned).unwrap(), expected);
            assert!(expected.chars().count() <= 80);
            std::fs::remove_file(path).unwrap();
            assert!(input_preview(&source).is_err());
        }
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn share_helpers() {
        assert_eq!(short("abcdef1234567890"), "abcdef12");
        // preview picks the meaningful key
        let v = serde_json::json!({"file_path": "/a/b.rs", "old_string": "xxxx"});
        assert_eq!(input_preview(&v.into()).unwrap(), "/a/b.rs");
        let v2 = serde_json::json!({"weird": 1});
        assert!(input_preview(&v2.into()).unwrap().contains("weird"));
    }

    #[test]
    fn lazy_html_text_matches_owned_and_rejects_changed_sources() -> anyhow::Result<()> {
        let world = crate::test_world("html-lazy-card");
        let path = world.base.join("body.json");
        let body = format!(
            "{}<script>bad</script> `λ`\n\n```rust\n<unsafe>\n```",
            "ordinary ".repeat(2000)
        );
        std::fs::write(&path, serde_json::to_vec(&body)?)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let text = source
            .read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                view.source_text()
            })?
            .unwrap();
        let mut lazy = Message::new(Role::Assistant);
        lazy.content = vec![
            Block::Text { text: text.clone() },
            Block::Thinking {
                text: text.clone(),
                signature: None,
                encrypted: None,
                redacted: false,
            },
            Block::ToolResult {
                tool_use_id: "call".into(),
                content: text,
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            },
        ];
        let mut owned = lazy.clone();
        owned.materialize(&crate::Resolver::new(None))?;
        let mut before = String::new();
        let mut after = String::new();
        share_message(&owned, 0, &mut before, &crate::Resolver::new(None))?;
        share_message(&lazy, 0, &mut after, &crate::Resolver::new(None))?;
        assert_eq!(before, after);
        assert!(!after.contains("<script>bad"));
        assert!(after.contains("&lt;script&gt;bad"));
        std::fs::write(&path, b"changed")?;
        assert!(share_message(&lazy, 0, &mut String::new(), &crate::Resolver::new(None)).is_err());
        Ok(())
    }
}
