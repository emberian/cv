//! Keep only facts used by the existing fidelity reducer, never payload bodies.
use super::*;
use crate::stream::{Flow, MessageSink};

pub(super) struct Accumulator {
    target: Harness,
    thinking: ThinkingMode,
    features: Features,
    // The answered-call rule ([`WriteRules::answered_calls_only`]) needs future IDs, but not
    // argument/output bodies.
    answered_only: bool,
    pending: Vec<(Message, Option<Harness>)>,
    answered: std::collections::HashSet<String>,
    resolver: crate::lazy::Resolver,
    latch: crate::stream::Latch,
    /// The harness the session was read from, on the side a writer is about to write
    /// ([`Features::note`]); `None` on the side read back.
    written_from: Option<Harness>,
}
impl Accumulator {
    pub(super) fn new(
        target: Harness,
        thinking: ThinkingMode,
        resolver: crate::lazy::Resolver,
        written_from: Option<Harness>,
    ) -> Self {
        Self {
            written_from,
            target,
            thinking,
            features: Features::default(),
            answered_only: write_rules(target).answered_calls_only,
            pending: Vec::new(),
            answered: Default::default(),
            resolver,
            latch: Default::default(),
        }
    }
    pub(super) fn note(&mut self, message: &Message) -> Result<()> {
        self.note_from(message, self.written_from)
    }
    pub(super) fn note_from(&mut self, message: &Message, written_from: Option<Harness>) -> Result<()> {
        let facts = project(message, &self.resolver, self.target)?;
        if self.answered_only {
            for block in &facts.content {
                if let Block::ToolResult { tool_use_id, .. } = block {
                    self.answered.insert(tool_use_id.clone());
                }
            }
        }
        if self.answered_only && facts.content.iter().any(|b| matches!(b, Block::ToolUse { .. })) {
            self.pending.push((facts, written_from));
        } else {
            self.features
                .note(self.target, self.thinking, &facts, false, &|_| false, written_from);
        }
        Ok(())
    }
    pub(super) fn finish(mut self, metadata: &Session) -> Features {
        if self.answered_only {
            for (message, written_from) in &self.pending {
                self.features.note(
                    self.target,
                    self.thinking,
                    message,
                    metadata.model.is_some(),
                    &|b| matches!(b, Block::ToolUse { id, .. } if !self.answered.contains(id.as_str())),
                    *written_from,
                );
            }
        }
        if metadata.model.is_some() {
            // Adapters can establish the session default at EOF. It applies to every eligible turn.
            self.features.model_eff = self.features.role_counts.values().sum();
        }
        self.features
    }
}
impl MessageSink for Accumulator {
    fn message(&mut self, message: Message) -> Flow {
        if self.latch.failed() {
            return Flow::Stop;
        }
        match self.note(&message) {
            Ok(()) => Flow::Continue,
            Err(error) => {
                self.latch.fail(error);
                Flow::Stop
            }
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.latch.take("fidelity projection")
    }
    fn meta(&mut self, session: &Session) {
        self.resolver = session.resolver();
    }
}

/// The features of `session` as `target`'s writer is about to write it.
pub(in crate::emit) fn source_features(target: Harness, thinking: ThinkingMode, session: &Session) -> Result<Features> {
    let sources = crate::splice::message_harnesses(session);
    let mut accumulator = Accumulator::new(target, thinking, session.resolver(), Some(session.harness));
    for (i, message) in session.messages.iter().enumerate() {
        let from = sources
            .as_ref()
            .and_then(|s| s.get(i).copied())
            .unwrap_or(session.harness);
        accumulator.note_from(message, Some(from))?;
    }
    Ok(accumulator.finish(session))
}

/// The features of `session` as `target`'s reader read it back.
#[cfg(test)]
pub(in crate::emit) fn read_features(target: Harness, thinking: ThinkingMode, session: &Session) -> Result<Features> {
    let mut accumulator = Accumulator::new(target, thinking, session.resolver(), None);
    for message in &session.messages {
        accumulator.note(message)?;
    }
    Ok(accumulator.finish(session))
}

/// Preserve the first nonblank character classification consumed by `is_client_command`:
/// `/` and `?` are commands, `x` is other nonblank text, and empty is blank.
pub(super) fn marker(text: &crate::lazy::Text, resolver: &crate::lazy::Resolver) -> Result<String> {
    Ok(match crate::emit::thinking::first_character(text, resolver, true)? {
        Some(ch @ ('/' | '?')) => ch.to_string(),
        Some(_) => "x".into(),
        None => String::new(),
    })
}

/// `message` reduced to the facts the feature counts read, written in a small presence-marker
/// language over the IR's own types rather than a type of its own: an optional field present in
/// the source is `Some("")` (an id, a model, a signature) or `Some(Value::Null)` (details), absent
/// stays `None`; a text is its [`marker`] (`/`, `?`, `x` or empty, first block only) and every other
/// body is emptied once validated; a tool id survives only where the target answers calls by id
/// ([`write_rules`]`.answered_calls_only`); a result's `tool_name` is `x` when nonempty; images and
/// files keep only their kind.
fn project(message: &Message, resolver: &crate::lazy::Resolver, target: Harness) -> Result<Message> {
    let rules = write_rules(target);
    let keep_ids = rules.answered_calls_only;
    let mut facts = if message.is_synthesized() {
        Message::synthesized(message.kind, message.origin)
    } else {
        Message::new(message.role)
    };
    facts.role = message.role;
    facts.id = message.id.as_ref().map(|_| String::new());
    facts.kind = message.kind;
    facts.origin = message.origin;
    facts.timestamp = message.timestamp;
    facts.model = message.model.as_ref().map(|_| String::new());
    facts.usage = message.usage.clone();
    if message.is_carrier() {
        // An empty record: enough for `is_carrier` and for the Claude writer's `system_fate`.
        facts
            .extra
            .insert(crate::ir::CARRIER_KEY.into(), Value::Object(Default::default()));
    }
    let mut text_started = false;
    for block in &message.content {
        facts.content.push(match block {
            Block::Text { text, .. } => {
                let mut value = if rules.reroles_client_commands && message.kind == MessageKind::Prompt {
                    marker(text, resolver)?
                } else {
                    validate_text(text, resolver)?;
                    String::new()
                };
                // Only the first nonblank text character determines the client-command re-role.
                if text_started {
                    value.clear();
                } else {
                    text_started = !value.is_empty();
                }
                Block::from_text(value.into())
            }
            Block::Thinking {
                text,
                signature,
                encrypted,
                redacted,
                ..
            } => Block::Thinking {
                text: marker(text, resolver)?.into(),
                signature: signature.as_ref().map(|_| String::new()),
                encrypted: encrypted.as_ref().map(|_| String::new()),
                redacted: *redacted,
                extensions: Default::default(),
            },
            Block::ToolUse { id, input, .. } => {
                // Preserve the old source-error boundary even though feature counts ignore args.
                input.with_view(|_| Ok(()))?;
                Block::ToolUse {
                    id: if keep_ids { id.clone() } else { String::new() },
                    name: String::new(),
                    input: Value::Null.into(),
                    namespace: None,
                    extensions: Default::default(),
                }
            }
            Block::ToolResult {
                tool_use_id,
                content,
                tool_name,
                is_error,
                details,
                ..
            } => {
                validate_text(content, resolver)?;
                Block::ToolResult {
                    tool_use_id: if keep_ids { tool_use_id.clone() } else { String::new() },
                    content: "".into(),
                    tool_name: tool_name
                        .as_ref()
                        .map(|name| if name.is_empty() { String::new() } else { "x".into() }),
                    is_error: *is_error,
                    status: None,
                    details: details.as_ref().map(|_| Value::Null),
                    extensions: Default::default(),
                }
            }
            Block::Opaque { block_type, .. } => Block::Opaque {
                block_type: block_type.clone(),
                value: Value::Null,
            },
            Block::Image { .. } => Block::Image {
                media_type: None,
                data_ref: None,
                extensions: Default::default(),
            },
            Block::File { .. } => Block::File {
                mime: None,
                path: None,
                source: None,
                extensions: Default::default(),
            },
        });
    }
    Ok(facts)
}

fn validate_text(text: &crate::lazy::Text, resolver: &crate::lazy::Resolver) -> Result<()> {
    match text {
        crate::lazy::Text::Inline(_) => Ok(()),
        crate::lazy::Text::Source(source) => source.json().with_view(|view| {
            anyhow::ensure!(view.is_string()?, "expected source text string");
            Ok(())
        }),
        crate::lazy::Text::Span(_) => text.try_for_each_chunk(resolver, 64 * 1024, |_| Ok(())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_preserves_future_answers_final_model_and_client_commands() -> Result<()> {
        let mut session = Session::new("projection", Harness::Claude);
        let mut call = Message::new(Role::Assistant);
        call.content.push(Block::ToolUse {
            id: "later".into(),
            name: "tool".into(),
            input: serde_json::json!({"large": "ignored"}).into(),
            namespace: None,
            extensions: Default::default(),
        });
        let mut orphan = call.clone();
        if let Block::ToolUse { id, .. } = &mut orphan.content[0] {
            *id = "orphan".into();
        }
        let mut result = Message::new(Role::Tool);
        result.content.push(Block::ToolResult {
            tool_use_id: "later".into(),
            content: "output".into(),
            tool_name: Some("tool".into()),
            is_error: true,
            status: None,
            details: Some(Value::Null),
            extensions: Default::default(),
        });
        let mut command = Message::new(Role::User);
        command.content = vec![Block::from_text(" \n".into()), Block::from_text(" ?help".into())];
        command.model = Some("explicit".into());
        let mut signed = Message::new(Role::Assistant);
        signed.content.push(Block::Thinking {
            text: "\u{2003}".into(),
            signature: Some(String::new()),
            encrypted: None,
            redacted: true,
            extensions: Default::default(),
        });
        let mut carrier = Message::new(Role::System);
        carrier.kind = MessageKind::Carrier;
        session.messages = vec![result, orphan, command, signed, carrier, call];
        let mut golden = String::new();
        for model in [None, Some("late-default".into())] {
            session.model = model;
            for target in Harness::ALL {
                for thinking in [ThinkingMode::Native, ThinkingMode::Text, ThinkingMode::Drop] {
                    golden.push_str(&format!(
                        "{:?}/{target}/{thinking} {:?}\n",
                        session.model,
                        source_features(target, thinking, &session)?
                    ));
                }
            }
        }
        assert_eq!(
            golden,
            include_str!("../../../tests/fixtures/emit-frozen/fidelity-features.txt")
        );
        session.model = None;
        let facts = source_features(Harness::Claude, ThinkingMode::Native, &session)?;
        assert_eq!(facts.tool_use, 1, "only the answered call survives");
        assert_eq!(facts.tool_result, 1);
        assert_eq!(facts.results_with_name, 1);
        assert_eq!(facts.results_is_error, 1);
        assert_eq!(facts.results_with_details, 1);
        assert_eq!(facts.thinking_text, 0, "Unicode whitespace is blank");
        assert_eq!(facts.model_eff, 1, "the command has an explicit model");
        session.model = Some("late-default".into());
        let facts = source_features(Harness::Claude, ThinkingMode::Native, &session)?;
        assert_eq!(facts.model_eff, facts.role_counts.values().sum::<usize>());
        assert!(!facts.kind_counts.contains_key("carrier"));
        Ok(())
    }
}
