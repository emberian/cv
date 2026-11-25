//! Legacy in-workspace chat tabs (`workbench.panel.aichat.view.aichat.chatdata` in a workspace's
//! `ItemTable`): `{tabs:[{tabId, chatTitle, bubbles:[{type:"user"|"ai", text}]}]}`, read through
//! the same cell views as the global store.
use super::*;
use crate::harness::store::{document::Fields, sqlite::Cells};
use crate::json::{JsonArray, JsonView};

/// Legacy key holding an array of chat tabs in the workspace ItemTable.
pub(super) const CHAT_KEY: &str = "workbench.panel.aichat.view.aichat.chatdata";

/// One tab's `tabId`, `chatTitle` and bubbles.
type Tab<'a> = (Option<String>, Option<String>, Option<JsonArray<'a>>);

/// None for a tab that is not an object.
fn tab(view: JsonView<'_>) -> Result<Option<Tab<'_>>> {
    let Some(fields) = Fields::parse(view, &["tabId", "chatTitle", "bubbles"])? else {
        return Ok(None);
    };
    let bubbles = fields.get("bubbles").map(|v| v.array()).transpose()?.flatten();
    Ok(Some((fields.string("tabId")?, fields.string("chatTitle")?, bubbles)))
}

fn tabs(root: JsonView<'_>) -> Result<Option<JsonArray<'_>>> {
    let Some(fields) = Fields::parse(root, &["tabs"])? else {
        return Ok(None);
    };
    Ok(fields.get("tabs").map(|v| v.array()).transpose()?.flatten())
}

pub(super) fn discover(conn: &Connection, db: &Path, cwd: Option<&Path>, out: &mut Vec<SessionRef>) -> Result<()> {
    let cells = Cells::new(conn, db, &crate::source::ReadSet::default(), "ItemTable")?;
    let Some(cell) = source::cell(conn, &cells, "ItemTable", CHAT_KEY)? else {
        return Ok(());
    };
    cells.read(cell, |root| {
        for view in tabs(root)?.into_iter().flatten() {
            let Some((Some(tab_id), title, bubbles)) = tab(view?)? else {
                continue;
            };
            let mut count = 0;
            for view in bubbles.into_iter().flatten() {
                count += usize::from(bubble(view?, true)?.is_some_and(|message| message.is_turn()));
            }
            if count == 0 {
                continue;
            }
            out.push(SessionRef {
                cwd: cwd.map(Path::to_path_buf),
                title: title
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| crate::ir::truncate(&s, crate::projection::Head::TITLE)),
                message_count: Some(count),
                ..SessionRef::new(format!("legacy:{tab_id}"), Harness::Cursor, db.to_path_buf())
            });
        }
        Ok(())
    })?;
    Ok(())
}

pub(super) fn stream(
    conn: &Connection,
    reads: &crate::source::ReadSet,
    r: &SessionRef,
    tab_id: &str,
    spans: bool,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    // Keep cell lookup and subsequent body reads on one SQLite snapshot; reuse an existing
    // transaction when the caller already opened one.
    let _snapshot = crate::harness::store::sqlite::snapshot(conn)?;
    let cells = Cells::new(conn, &r.path, reads, "ItemTable")?;
    let cell = source::cell(conn, &cells, "ItemTable", CHAT_KEY)?.context("legacy chatdata missing")?;
    let session = cells
        .read(cell, |root| {
            let tabs = tabs(root)?.context("legacy chatdata has no tabs")?;
            let mut selected = None;
            for view in tabs {
                if let Some((Some(id), title, bubbles)) = tab(view?)? {
                    if id == tab_id {
                        selected = Some((title, bubbles));
                        break;
                    }
                }
            }
            let (title, bubbles) = selected.with_context(|| format!("legacy tab {tab_id} not found"))?;
            let session = Session {
                cwd: r.cwd.clone(),
                title: title.filter(|s| !s.trim().is_empty()).or_else(|| r.title.clone()),
                created_at: r.created_at,
                updated_at: r.updated_at,
                source_path: Some(r.path.clone()),
                ..Session::new(r.id.clone(), Harness::Cursor)
            };
            sink.meta(&session);
            for bubble in bubbles.into_iter().flatten() {
                if let Some(message) = self::bubble(bubble?, spans)? {
                    if sink.message(message) == Flow::Stop {
                        break;
                    }
                }
            }
            Ok(session)
        })?
        .context("legacy chatdata JSON")?;
    cells.check()?;
    Ok(session)
}

/// Legacy bubbles use a string `type` ("user"/"ai") and a `text` field.
fn bubble(view: JsonView<'_>, spans: bool) -> Result<Option<Message>> {
    let Some(fields) = Fields::parse(view, &["type", "text"])? else {
        return Ok(None);
    };
    let role = match fields.string("type")?.as_deref() {
        Some("user") => Role::User,
        _ => Role::Assistant,
    };
    let mut m = Message::new(role);
    if let Some(text) = fields.get("text").map(|v| v.lazy_text(spans)).transpose()?.flatten() {
        if !matches!(&text, crate::lazy::Text::Inline(s) if s.is_empty()) {
            m.content.push(Block::from_text(text));
        }
    }
    Ok((!m.content.is_empty()).then_some(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tabs_select_the_last_member_and_skip_malformed_tabs() {
        let world = crate::test_world("cursor-legacy-tabs");
        for (index, (text, expected)) in [
            (
                r#"{"tabs":[{"tabId":"a","chatTitle":"A","bubbles":[{"type":"user","text":"1"},2]},{"tabId":"a","bubbles":[3]}]}"#,
                Some(vec![("a", Some("A"), Some(1))]),
            ),
            (
                r#"{"tabs":[{"tabId":"obsolete","bubbles":[1]}],"tabs":[{"tabId":"a","bubbles":[2],"bubbles":[3,4]}]}"#,
                Some(vec![]),
            ),
            (r#"{"tabs":[{"tabId":"a","bubbles":[1],"bubbles":false}]}"#, Some(vec![])),
            (r#"{"tabs":[null,1,[],{"tabId":"a","bubbles":[]}]}"#, Some(vec![])),
            (r#"{"tabs":[{"tabId":"a","bubbles":[1]}],"ignored":"\ud800"}"#, None),
            (r#"{"tabs":[{"tabId":"a","bubbles":[1]}]} trailing"#, None),
            ("null", Some(vec![])),
            (r#"{"tabs":false}"#, Some(vec![])),
        ]
        .into_iter()
        .enumerate()
        {
            let path = world.base.join(format!("{index}.vscdb"));
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)")
                .unwrap();
            conn.execute("INSERT INTO ItemTable VALUES (?1, ?2)", rusqlite::params![CHAT_KEY, text])
                .unwrap();
            let mut out = Vec::new();
            let result = discover(&conn, &path, None, &mut out);
            let Some(expected) = expected else {
                // Not exactly one JSON document: no tabs.
                assert!(out.is_empty(), "{text}");
                continue;
            };
            result.unwrap();
            let actual: Vec<_> = out
                .iter()
                .map(|r| (r.id.strip_prefix("legacy:").unwrap(), r.title.as_deref(), r.message_count))
                .collect();
            assert_eq!(actual, expected, "{text}");
        }
    }
}
