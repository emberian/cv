//! Audience projections. Only explicitly selected fields are evaluated at each tier.
use crate::{Block, Harness, MessageKind, Role, Session};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum DisclosureTier {
    Presence = 0,
    Activity = 1,
    Conversation = 2,
    Full = 3,
}

impl std::str::FromStr for DisclosureTier {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "0" | "presence" => Self::Presence,
            "1" | "activity" => Self::Activity,
            "2" | "conversation" => Self::Conversation,
            "3" | "full" => Self::Full,
            _ => anyhow::bail!("unknown reader disclosure tier"),
        })
    }
}

#[derive(Serialize)]
pub struct Presence {
    pub id: String,
    pub harness: Harness,
}

#[derive(Serialize)]
pub struct Activity {
    pub presence: Presence,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct Conversation {
    pub activity: Activity,
    pub messages: Vec<ConversationMessage>,
}

#[derive(Serialize)]
pub struct ConversationMessage {
    pub role: Role,
    pub text: Vec<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum Disclosed {
    Presence(Presence),
    Activity(Activity),
    Conversation(Conversation),
    Full(Box<Session>),
}

pub fn disclose(tier: DisclosureTier, session: &Session) -> anyhow::Result<Disclosed> {
    if tier == DisclosureTier::Full {
        let (full, _) = crate::redact::scrub_session_with_profile(session, &Default::default())?;
        return Ok(Disclosed::Full(Box::new(full)));
    }
    let presence = Presence {
        id: crate::redact::redact_text(&session.id),
        harness: session.harness,
    };
    if tier == DisclosureTier::Presence {
        return Ok(Disclosed::Presence(presence));
    }
    let activity = Activity {
        presence,
        created_at: session.created_at,
        updated_at: session.updated_at,
    };
    if tier == DisclosureTier::Activity {
        return Ok(Disclosed::Activity(activity));
    }
    let mut messages = Vec::new();
    let resolver = session.resolver();
    for message in &session.messages {
        if !matches!(
            (message.role, message.kind, message.origin),
            (Role::User, MessageKind::Prompt, crate::Origin::Human) | (Role::Assistant, MessageKind::Reply, _)
        ) {
            continue;
        }
        let mut text = Vec::new();
        for block in &message.content {
            if let Block::Text { text: body, .. } = block {
                text.push(crate::redact::redact_text(&body.try_resolve(&resolver)?));
            }
        }
        if !text.is_empty() {
            messages.push(ConversationMessage {
                role: message.role,
                text,
            });
        }
    }
    Ok(Disclosed::Conversation(Conversation { activity, messages }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    /// #tooth Differences above each grant are unobservable in its encoded response.
    #[test]
    fn disclose_above_tier_fields_cannot_change_bytes() -> anyhow::Result<()> {
        let original = json!({"id":"test","harness":"claude","messages":[{"role":"user","kind":"prompt","origin":"human","content":[{"type":"text","text":"hello"}]}]});
        let bytes = |tier, value| -> anyhow::Result<Vec<u8>> {
            Ok(serde_json::to_vec(&disclose(tier, &serde_json::from_value(value)?)?)?)
        };
        for tier in [
            DisclosureTier::Presence,
            DisclosureTier::Activity,
            DisclosureTier::Conversation,
        ] {
            let mut different = original.clone();
            for (key, v) in [
                ("principal", json!({"identity":"private"})),
                ("title", json!("private")),
                ("model", json!("private")),
                ("system_prompt", json!("private")),
                ("git", json!({"branch":"private","commit":"private","remote":"private"})),
                ("cwd", json!("/private")),
                ("source_path", json!("/private")),
                (
                    "lineage",
                    json!({"forked_from":"private","parent":"private","spawned_by_tool_use":"private","continued_in":"private","continues":"private","agent_path":"private"}),
                ),
                ("extra", json!({"new":"private"})),
            ] {
                different[key] = v;
            }
            let m = &mut different["messages"][0];
            for key in ["id", "parent_id", "model"] {
                m[key] = json!("private");
            }
            if tier != DisclosureTier::Conversation {
                m["origin"] = json!("hook");
            }
            m["timestamp"] = json!("2020-01-01T00:00:00Z");
            m["extra"] = json!({"future":"private"});
            m["usage"] = json!({"input_tokens":12,"output_tokens":42,"cache_read_tokens":9,"cache_creation_tokens":2,"reasoning_tokens":8,"cost_usd":0.3});
            for b in [
                json!({"type":"thinking","text":"private","signature":"private","encrypted":"private","redacted":true}),
                json!({"type":"tool_use","id":"private","name":"private","namespace":"private","input":{"private":1}}),
                json!({"type":"tool_result","content":"private","tool_use_id":"private","details":{"private":1},"tool_name":"private","is_error":true,"status":"private"}),
                json!({"type":"image","data_ref":"private","media_type":"private"}),
                json!({"type":"file","path":"private","source":"private","mime":"private"}),
            ] {
                m["content"].as_array_mut().unwrap().push(b);
            }
            if tier != DisclosureTier::Conversation {
                m["role"] = json!("assistant");
                m["kind"] = json!("reply");
                m["content"][0]["text"] = json!("private");
            }
            different["messages"].as_array_mut().unwrap().push(json!({"role":"assistant","kind":"reply","origin":"model","content":[{"type":"thinking","text":"private"}]}));
            if tier == DisclosureTier::Presence {
                different["updated_at"] = json!("2020-01-01T00:00:00Z");
                different["created_at"] = different["updated_at"].clone();
            }
            let expected = bytes(tier, original.clone())?;
            let mut changed: Session = serde_json::from_value(different)?;
            changed.source_stamp = crate::lazy::SourceStamp::capture(std::path::Path::new(file!()));
            crate::ir::CompleteRecord::attach_replay(
                &mut changed.messages,
                changed.harness,
                json!({"secret":"hidden"}).into(),
                Default::default(),
            );
            assert_eq!(expected, serde_json::to_vec(&disclose(tier, &changed)?)?);
            // Each grant still observes changes to its allowed fields.
            match tier {
                DisclosureTier::Presence => changed.id.push('x'),
                DisclosureTier::Activity => changed.created_at = chrono::DateTime::from_timestamp(1, 0),
                DisclosureTier::Conversation => changed.messages[0].content.clear(),
                DisclosureTier::Full => unreachable!(),
            }
            assert_ne!(expected, serde_json::to_vec(&disclose(tier, &changed)?)?);
        }

        // Imported or hook-seeded user prompts are not human turns and must not leak at Conversation tier.
        for non_human in ["import", "hook", "harness", "subagent"] {
            let seeded: Session = serde_json::from_value(json!({
                "id": "seeded",
                "harness": "claude",
                "messages": [
                    {"role": "user", "kind": "prompt", "origin": non_human, "content": [{"type": "text", "text": "imported seed"}]},
                    {"role": "assistant", "kind": "reply", "origin": "model", "content": [{"type": "text", "text": "visible reply"}]}
                ]
            }))?;
            let Disclosed::Conversation(conv) = disclose(DisclosureTier::Conversation, &seeded)? else {
                unreachable!()
            };
            assert_eq!(conv.messages.len(), 1, "origin={non_human} leaked as human prompt");
            assert_eq!(conv.messages[0].role, Role::Assistant);
        }
        Ok(())
    }
}
