//! Evidence about a read's sources and known omissions, independent of rendering demand.
//! No entry means unassessed; an empty limitation set never asserts complete native fidelity.
use crate::Session;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Extensible identifiers preserve future source/limitation values through older cv readers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadCoverage {
    #[serde(default)]
    pub sources: BTreeSet<String>,
    #[serde(default)]
    pub limitations: BTreeSet<String>,
}

impl ReadCoverage {
    pub const EXPORTED_HISTORY: &'static str = "exported_history";
    pub const NATIVE_STORE: &'static str = "native_store";
    pub const MEMBER_REGISTRY: &'static str = "member_registry";
    pub const METADATA_ONLY: &'static str = "metadata_only";
    pub const MISSING_HISTORY: &'static str = "missing_history";
    pub const UNRECONCILED_HISTORY: &'static str = "unreconciled_history";
    pub const MISSING_PAYLOADS: &'static str = "missing_payloads";
    pub const MISSING_ATTACHMENTS: &'static str = "missing_attachments";
    pub const UNSUPPORTED_NATIVE_FIELDS: &'static str = "unsupported_native_fields";
    pub const INFERRED_TOOL_IDENTITY: &'static str = "inferred_tool_identity";

    pub fn of_session(session: &Session) -> Option<Self> {
        serde_json::from_value(session.cv_extra()?.get("read_coverage")?.clone()).ok()
    }

    /// Publish evidence at every demand tier, including LEAN/FACTS. Adapters should send
    /// metadata before body output and update it if later records reveal additional omissions.
    pub fn attach(&self, session: &mut Session) {
        session
            .cv_extra_mut()
            .insert("read_coverage".into(), serde_json::json!(self));
    }

    /// A single compact line, with consequences rather than an unsupported completeness score.
    pub fn summary(&self) -> String {
        let source = |s: &str| match s {
            Self::EXPORTED_HISTORY => "exported history".to_owned(),
            Self::NATIVE_STORE => "native store".to_owned(),
            Self::MEMBER_REGISTRY => "member registry".to_owned(),
            _ => "unrecognized source (see JSON metadata)".to_owned(),
        };
        let limitation = |s: &str| match s {
            "opaque_message_kind" => "uninterpreted OpenSession message kinds retained".into(),
            "opaque_block_type" => "uninterpreted OpenSession block types retained".into(),
            Self::METADATA_ONLY => "metadata only; no conversation text".to_owned(),
            Self::MISSING_HISTORY => "some history is unavailable".to_owned(),
            Self::UNRECONCILED_HISTORY => "newer native history could not be reconciled".to_owned(),
            Self::MISSING_PAYLOADS => "some recorded payloads are unavailable".to_owned(),
            Self::MISSING_ATTACHMENTS => "some referenced attachments are unavailable".to_owned(),
            Self::UNSUPPORTED_NATIVE_FIELDS => "some native fields are not interpreted".to_owned(),
            Self::INFERRED_TOOL_IDENTITY => "tool associations are inferred".to_owned(),
            _ => "unrecognized limitation (see JSON metadata)".to_owned(),
        };
        let mut parts = self.sources.iter().map(|s| source(s)).collect::<Vec<_>>();
        parts.extend(self.limitations.iter().map(|s| limitation(s)));
        if parts.is_empty() {
            "coverage unassessed".into()
        } else {
            parts.join("; ")
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionCoverage {
    pub harness: crate::Harness,
    pub id: String,
    pub coverage: ReadCoverage,
}

impl SessionCoverage {
    pub fn of_session(session: &Session) -> Option<Self> {
        Some(Self {
            harness: session.harness,
            id: session.id.clone(),
            coverage: ReadCoverage::of_session(session)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_values_round_trip_without_claiming_completeness() {
        let mut session = Session::new("x", crate::Harness::OpenSession);
        assert_eq!(ReadCoverage::of_session(&session), None);
        let coverage: ReadCoverage = serde_json::from_value(serde_json::json!({
            "sources":["native_store"], "limitations":["future_gap\nline"]
        }))
        .unwrap();
        coverage.attach(&mut session);
        let restored: Session = serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
        assert_eq!(ReadCoverage::of_session(&restored), Some(coverage.clone()));
        assert!(!coverage.summary().contains('\n'));
        assert!(coverage.summary().contains("unrecognized limitation"));
        assert_eq!(ReadCoverage::default().summary(), "coverage unassessed");
    }
}
