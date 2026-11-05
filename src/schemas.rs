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
