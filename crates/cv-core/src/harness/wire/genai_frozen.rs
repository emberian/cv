//! Independent decoder snapshots captured before the GenAI grammar was shared.
use crate::{lazy::Resolver, Block, Text};
use anyhow::Result;
use serde_json::{json, Value};
pub(crate) fn cases() -> Vec<Value> {
    let long = "λ\n\"".repeat(3000);
    vec![
        json!([]),
        json!([null, 3, "", "bare"]),
        json!([{"text":""},{"text":"plain"},{"text":"thought","thought":true,"thoughtSignature":"signed"}]),
        json!([{"functionCall":{},"functionResponse":{}},{"functionCall":null,"text":"fallback"},{"functionCall":3,"functionResponse":{},"text":"fallback"}]),
        json!([{"functionResponse":null,"text":"fallback"},{"functionResponse":3},{"functionResponse":{},"inlineData":{}}]),
        json!([{"inlineData":null,"fileData":{"mimeType":"text/plain","fileUri":"file:x"},"text":"fallback"},{"inlineData":3,"text":"fallback"},{"inlineData":{"mimeType":"image/png","data":"ignored"}}]),
        json!([{"fileData":null,"text":"fallback"},{"fileData":3,"text":"fallback"},{"fileData":{"mimeType":"text/plain","fileUri":"file:x"}}]),
        json!([{"functionCall":{"name":"tool_call","id":"call","args":{"name":"Read","arguments":{"path":"a"}}}},{"functionCall":{"args":null}}]),
        json!([{"functionResponse":{"name":"Read","response":{"output":"ok","error":null}}},{"functionResponse":{"response":{"error":"bad"}}},{"functionResponse":{"response":null}},{"functionResponse":{"response":[1,"x"]}}]),
        json!([{"text":long,"thought":true,"thoughtSignature":"large-signature"},{"text":long},{"functionCall":{"args":{"payload":long}}},{"functionResponse":{"response":{"output":long}}}]),
        json!([{"text":17,"thought":true},{"text":" x","thought":"true"},{"text":"  <system-reminder>hook","thoughtSignature":"ignored"}]),
    ]
}
pub(crate) fn row(mut blocks: Vec<Block>, flag: bool, resolver: &Resolver) -> Result<Value> {
    let mut carriers = Vec::new();
    for block in &mut blocks {
        let text = match block {
            Block::Text { text } | Block::Thinking { text, .. } | Block::ToolResult { content: text, .. } => Some(text),
            _ => None,
        };
        if let Some(text) = text {
            carriers.push(match text {
                Text::Inline(_) => "inline",
                Text::Span(_) => "span",
                Text::Source(_) => "source",
            });
            *text = Text::Inline(text.try_resolve(resolver)?.into_owned());
        }
        if let Block::ToolUse { input, .. } = block {
            carriers.push(match input {
                crate::Json::Owned(_) => "owned-json",
                crate::Json::Source(_) => "source-json",
            });
            input.make_owned()?;
        }
    }
    let bytes = serde_json::to_vec(&blocks)?;
    Ok(json!({"resolved_sha256":crate::digest::sha256_hex(&bytes),"carriers":carriers,"response":flag}))
}
pub(crate) fn assert_frozen(rows: Vec<Value>, expected: &str, fixture_rel: &str) -> Result<()> {
    if std::env::var_os("CV_REGEN_CORPUS").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture_rel);
        let mut text = String::new();
        for row in &rows {
            text.push_str(&serde_json::to_string(row)?);
            text.push('\n');
        }
        std::fs::write(path, text)?;
        return Ok(());
    }
    let expected: Vec<Value> = expected
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(rows, expected);
    Ok(())
}
