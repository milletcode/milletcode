use serde::{Deserialize, Serialize};

use super::types::UserQuestionItem;

/// One in-band control request parked for a human decision, mirrored in a
/// central registry so any surface (message panel, badge, REST bootstrap)
/// can list what is currently waiting across all sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingControlInfo {
    pub kind: PendingKind,
    pub project_name: Option<String>,
    pub agent_name: Option<String>,
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PendingKind {
    Approval {
        prompt: String,
        options: Vec<String>,
        tool_name: Option<String>,
        request_id: Option<String>,
    },
    Question {
        tool_use_id: String,
        request_id: String,
        questions: Vec<UserQuestionItem>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_approval() {
        let info = PendingControlInfo {
            kind: PendingKind::Approval {
                prompt: "rm -rf /tmp/x".into(),
                options: vec!["approve".into(), "deny".into()],
                tool_name: Some("Bash".into()),
                request_id: Some("ctrl_001".into()),
            },
            project_name: Some("millet-stock".into()),
            agent_name: Some("coder".into()),
            ts: 1_700_000_000,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"Approval\""));
        let back: PendingControlInfo = serde_json::from_str(&json).unwrap();
        match back.kind {
            PendingKind::Approval { prompt, options, tool_name, request_id } => {
                assert_eq!(prompt, "rm -rf /tmp/x");
                assert_eq!(options, vec!["approve", "deny"]);
                assert_eq!(tool_name.as_deref(), Some("Bash"));
                assert_eq!(request_id.as_deref(), Some("ctrl_001"));
            }
            _ => panic!("wrong variant after roundtrip"),
        }
        assert_eq!(back.project_name.as_deref(), Some("millet-stock"));
        assert_eq!(back.ts, 1_700_000_000);
    }

    #[test]
    fn serde_roundtrip_question() {
        let info = PendingControlInfo {
            kind: PendingKind::Question {
                tool_use_id: "toolu_01".into(),
                request_id: "ctrl_002".into(),
                questions: vec![UserQuestionItem {
                    question: "Which DB?".into(),
                    header: "Database".into(),
                    options: vec![super::super::types::UserQuestionOption {
                        label: "Postgres".into(),
                        description: "recommended".into(),
                    }],
                }],
            },
            project_name: None,
            agent_name: None,
            ts: 42,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"Question\""));
        let back: PendingControlInfo = serde_json::from_str(&json).unwrap();
        match back.kind {
            PendingKind::Question { tool_use_id, request_id, questions } => {
                assert_eq!(tool_use_id, "toolu_01");
                assert_eq!(request_id, "ctrl_002");
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].options[0].label, "Postgres");
            }
            _ => panic!("wrong variant after roundtrip"),
        }
    }

    #[test]
    fn serde_optional_fields_default_to_none() {
        let info = PendingControlInfo {
            kind: PendingKind::Approval {
                prompt: String::new(),
                options: vec![],
                tool_name: None,
                request_id: None,
            },
            project_name: None,
            agent_name: None,
            ts: 0,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"tool_name\":null"));
        let back: PendingControlInfo = serde_json::from_str(&json).unwrap();
        match back.kind {
            PendingKind::Approval { tool_name, request_id, .. } => {
                assert!(tool_name.is_none());
                assert!(request_id.is_none());
            }
            _ => panic!("wrong variant after roundtrip"),
        }
    }
}
