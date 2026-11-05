//! Request and output models mirroring `app/schemas.py`.
//!
//! Output models reject unknown keys and missing required keys, as pydantic's
//! `extra="forbid"` does. `clarification_needed` is required even when null,
//! which is why it is a newtype instead of `Option<String>`.

use serde::{Deserialize, Deserializer, Serialize};

pub const STATUS_VALUES: [&str; 3] = ["complete", "partial", "needs_clarification"];
pub const CONFIDENCE_VALUES: [&str; 3] = ["high", "medium", "low"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Complete,
    Partial,
    NeedsClarification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// A string that may be null but must be present.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NullableString(pub Option<String>);

impl<'de> Deserialize<'de> for NullableString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NullableVisitor;

        impl<'de> serde::de::Visitor<'de> for NullableVisitor {
            type Value = NullableString;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string or null")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(NullableString(Some(value.to_string())))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(NullableString(Some(value)))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(NullableString(None))
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(NullableString(None))
            }

            fn visit_some<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                NullableString::deserialize(deserializer)
            }
        }

        // `deserialize_any` (not `Option`'s `deserialize_option`) keeps a
        // missing key an error: serde's missing-field sentinel maps to an error.
        deserializer.deserialize_any(NullableVisitor)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    pub agent: String,
    pub task: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationRequest {
    pub task: String,
    #[serde(default)]
    pub use_llm_planner: bool,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundRequest {
    pub message: String,
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String {
    "unknown".to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub claim: String,
    pub source_url: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOutput {
    pub agent: String,
    pub status: Status,
    pub task: String,
    pub summary: String,
    pub findings: Vec<Finding>,
    pub conflicts: Vec<String>,
    pub gaps: Vec<String>,
    pub clarification_needed: NullableString,
}

/// Strict JSON contract for the writer step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriterOutput {
    pub agent: String,
    pub status: Status,
    pub draft: String,
    pub facts_used: Vec<String>,
    pub notes: String,
    pub clarification_needed: NullableString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approved,
    NeedsChanges,
}

/// One problem found by the checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckerIssue {
    pub problem: String,
    pub location: String,
    pub suggestion: String,
}

/// Strict JSON contract for the checker step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckerOutput {
    pub agent: String,
    pub status: Status,
    pub verdict: Verdict,
    pub issues: Vec<CheckerIssue>,
    pub clarification_needed: NullableString,
}

/// The provider schema an orchestration step asks for, or `None` for unknown agents.
pub fn step_schema(agent: &str) -> Option<serde_json::Value> {
    match agent {
        "researcher" => Some(agent_output_schema()),
        "writer" => Some(serde_json::json!({
            "title": "WriterOutput",
            "type": "object",
            "additionalProperties": false,
            "required": [
                "agent", "status", "draft", "facts_used", "notes", "clarification_needed"
            ],
            "properties": {
                "agent": {"type": "string"},
                "status": {"type": "string", "enum": STATUS_VALUES},
                "draft": {"type": "string"},
                "facts_used": {"type": "array", "items": {"type": "string"}},
                "notes": {"type": "string"},
                "clarification_needed": {"type": ["string", "null"]}
            }
        })),
        "checker" => Some(serde_json::json!({
            "title": "CheckerOutput",
            "type": "object",
            "additionalProperties": false,
            "required": [
                "agent", "status", "verdict", "issues", "clarification_needed"
            ],
            "properties": {
                "agent": {"type": "string"},
                "status": {"type": "string", "enum": STATUS_VALUES},
                "verdict": {"type": "string", "enum": ["approved", "needs_changes"]},
                "issues": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["problem", "location", "suggestion"],
                        "properties": {
                            "problem": {"type": "string"},
                            "location": {"type": "string"},
                            "suggestion": {"type": "string"}
                        }
                    }
                },
                "clarification_needed": {"type": ["string", "null"]}
            }
        })),
        _ => None,
    }
}

/// Validate one step's output against its per-agent model and return normalized JSON.
pub fn validate_step_output(agent: &str, value: serde_json::Value) -> Result<serde_json::Value, String> {
    let parsed = match agent {
        "researcher" => serde_json::from_value::<AgentOutput>(value).map(|output| {
            serde_json::to_value(output).expect("validated output serializes")
        }),
        "writer" => serde_json::from_value::<WriterOutput>(value).map(|output| {
            serde_json::to_value(output).expect("validated output serializes")
        }),
        "checker" => serde_json::from_value::<CheckerOutput>(value).map(|output| {
            serde_json::to_value(output).expect("validated output serializes")
        }),
        _ => return Err(format!("unknown agent: {agent}")),
    };
    parsed.map_err(|error| format!("schema validation failed: {error}"))
}

/// Strict provider-side schema for the first rung of the response-format ladder.
pub fn agent_output_schema() -> serde_json::Value {
    serde_json::json!({
        "title": "AgentOutput",
        "type": "object",
        "additionalProperties": false,
        "required": [
            "agent", "status", "task", "summary", "findings",
            "conflicts", "gaps", "clarification_needed"
        ],
        "properties": {
            "agent": {"type": "string"},
            "status": {"type": "string", "enum": STATUS_VALUES},
            "task": {"type": "string"},
            "summary": {"type": "string"},
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["claim", "source_url", "confidence"],
                    "properties": {
                        "claim": {"type": "string"},
                        "source_url": {"type": "string"},
                        "confidence": {"type": "string", "enum": CONFIDENCE_VALUES}
                    }
                }
            },
            "conflicts": {"type": "array", "items": {"type": "string"}},
            "gaps": {"type": "array", "items": {"type": "string"}},
            "clarification_needed": {"type": ["string", "null"]}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> serde_json::Value {
        serde_json::json!({
            "agent": "researcher",
            "status": "complete",
            "task": "t",
            "summary": "s",
            "findings": [{"claim": "c", "source_url": "u", "confidence": "high"}],
            "conflicts": [],
            "gaps": [],
            "clarification_needed": null
        })
    }

    #[test]
    fn valid_output_round_trips_with_a_null_clarification() {
        let output: AgentOutput = serde_json::from_value(valid()).unwrap();
        assert_eq!(output.status, Status::Complete);
        assert_eq!(output.clarification_needed.0, None);
        assert!(serde_json::to_value(&output).unwrap()["clarification_needed"].is_null());
    }

    #[test]
    fn unknown_keys_and_missing_required_keys_are_rejected() {
        let mut extra = valid();
        extra["surprise"] = 1.into();
        assert!(serde_json::from_value::<AgentOutput>(extra).is_err());

        let mut missing = valid();
        missing
            .as_object_mut()
            .unwrap()
            .remove("clarification_needed");
        assert!(serde_json::from_value::<AgentOutput>(missing).is_err());

        let mut bad_enum = valid();
        bad_enum["status"] = "done".into();
        assert!(serde_json::from_value::<AgentOutput>(bad_enum).is_err());
    }

    #[test]
    fn findings_reject_unknown_keys() {
        let mut extra = valid();
        extra["findings"][0]["source"] = "x".into();
        assert!(serde_json::from_value::<AgentOutput>(extra).is_err());
    }
}
