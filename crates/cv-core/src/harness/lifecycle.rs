//! Typed lifecycle evidence decoded by adapters; consumers own call/job tracking.
use crate::tools::classify::ToolSemantic;

/// An observed terminal result does not imply the underlying work succeeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Complete,
    Failed,
    Stopped,
    Pending,
}

impl Status {
    pub fn from_native(status: &str) -> Self {
        match status.trim().to_ascii_lowercase().as_str() {
            "completed" | "complete" | "done" | "success" | "succeeded" => Self::Complete,
            "failed" | "error" | "invalid" => Self::Failed,
            "canceled" | "cancelled" | "killed" | "stopped" => Self::Stopped,
            _ => Self::Pending,
        }
    }
    pub fn is_terminal(self) -> bool {
        self != Self::Pending
    }
}

/// Bounded text is sufficient for safety preflight; full text may be supplied for facts.
pub struct ToolResult<'a> {
    /// Originating record, for provider-native metadata retained alongside result text.
    pub message: Option<&'a crate::ir::Message>,
    pub tool: &'a str,
    pub ops: &'a [ToolSemantic<'static>],
    pub body: &'a str,
    pub status: Option<&'a str>,
    pub is_error: bool,
    /// The result block's structured sidecar, when the adapter kept one at this read's tier.
    pub details: Option<&'a serde_json::Value>,
}

#[derive(Debug, Clone)]
pub enum Target {
    Call(String),
    Job(String),
}

#[derive(Debug, Clone)]
pub struct Update {
    pub target: Target,
    pub status: Status,
    pub output: Option<String>,
}

#[derive(Debug, Default)]
pub struct Observation {
    /// State of the tool invocation itself, separate from any jobs it started.
    pub status: Status,
    /// A single known job this invocation polls/controls, when unambiguous.
    pub acting_on: Option<String>,
    /// Newly observed jobs; attach these to the acting-on job's owner before applying updates.
    pub started_jobs: Vec<String>,
    pub updates: Vec<Update>,
    /// The envelope cannot establish child-job identity or completion.
    pub uncertain: bool,
    /// Decoded output for facts, when the native result wraps it in structured metadata.
    pub output: Option<String>,
    /// Native notification identity used to avoid mining queued/delivered duplicates twice.
    pub dedup: Option<String>,
}

impl Observation {
    pub fn result(result: &ToolResult<'_>) -> Self {
        Self {
            status: if result.is_error {
                Status::Failed
            } else {
                result.status.map(Status::from_native).unwrap_or_default()
            },
            ..Default::default()
        }
    }
}
