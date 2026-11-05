//! Runtime settings loaded once from `.env` at process start.
//!
//! Names, defaults and clamping mirror `app/config.py` so an existing `.env`
//! keeps working unchanged.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use tracing::warn;

/// Repository root, resolved at build time like the reference's `Path(__file__).parent.parent`.
pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// Prompt directory. `SOL_O_AGENTS_DIR` is a hermetic-test seam like `SOL_O_DB`.
pub fn agents_dir() -> PathBuf {
    std::env::var_os("SOL_O_AGENTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(ROOT).join("agents"))
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub llm_timeout_seconds: f64,
    pub llm_max_retries: i64,
    pub tool_timeout_seconds: f64,
    pub run_timeout_seconds: f64,
    pub max_agents: i64,
    pub require_approval: bool,
    pub auto_approve: bool,
    pub pending_ttl_hours: f64,
    pub max_input_chars: i64,
    pub agent_models: BTreeMap<String, String>,
    pub max_tool_turns: i64,
    pub max_result_chars: i64,
    pub max_result_items: i64,
    pub max_args_chars: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            llm_timeout_seconds: 30.0,
            llm_max_retries: 2,
            tool_timeout_seconds: 15.0,
            run_timeout_seconds: 180.0,
            max_agents: 7,
            require_approval: true,
            auto_approve: false,
            pending_ttl_hours: 24.0,
            max_input_chars: 8000,
            agent_models: BTreeMap::new(),
            max_tool_turns: 3,
            max_result_chars: 6000,
            max_result_items: 10,
            max_args_chars: 500,
        }
    }
}

/// Read the environment once; invalid values fall back to defaults with a warning.
pub fn load_settings() -> Settings {
    from_lookup(|name| std::env::var(name).ok(), &known_agents())
}

/// Agent ids that have a prompt file; those are the valid `AGENT_MODELS` keys.
pub fn known_agents() -> HashSet<String> {
    let mut agents = HashSet::new();
    let Ok(entries) = std::fs::read_dir(Path::new(ROOT).join("agents")) else {
        return agents;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("md") {
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                agents.insert(stem.to_string());
            }
        }
    }
    agents
}

pub fn from_lookup<F>(lookup: F, known: &HashSet<String>) -> Settings
where
    F: Fn(&str) -> Option<String>,
{
    let defaults = Settings::default();
    Settings {
        llm_timeout_seconds: parse_float(
            &lookup,
            "LLM_TIMEOUT_SECONDS",
            defaults.llm_timeout_seconds,
            0.001,
        ),
        llm_max_retries: parse_int(&lookup, "LLM_MAX_RETRIES", defaults.llm_max_retries, 0),
        tool_timeout_seconds: parse_float(
            &lookup,
            "TOOL_TIMEOUT_SECONDS",
            defaults.tool_timeout_seconds,
            0.001,
        ),
        run_timeout_seconds: parse_float(
            &lookup,
            "RUN_TIMEOUT_SECONDS",
            defaults.run_timeout_seconds,
            0.001,
        ),
        max_agents: parse_int(&lookup, "MAX_AGENTS", defaults.max_agents, 1),
        require_approval: parse_bool(&lookup, "REQUIRE_APPROVAL", defaults.require_approval),
        auto_approve: parse_bool(&lookup, "AUTO_APPROVE", defaults.auto_approve),
        pending_ttl_hours: parse_float(
            &lookup,
            "PENDING_TTL_HOURS",
            defaults.pending_ttl_hours,
            0.0,
        ),
        max_input_chars: parse_int(&lookup, "MAX_INPUT_CHARS", defaults.max_input_chars, 1),
        agent_models: parse_agent_models(&lookup("AGENT_MODELS").unwrap_or_default(), known),
        max_tool_turns: parse_int(&lookup, "MAX_TOOL_TURNS", defaults.max_tool_turns, 1),
        max_result_chars: parse_int(&lookup, "MAX_RESULT_CHARS", defaults.max_result_chars, 1),
        max_result_items: parse_int(&lookup, "MAX_RESULT_ITEMS", defaults.max_result_items, 1),
        max_args_chars: parse_int(&lookup, "MAX_ARGS_CHARS", defaults.max_args_chars, 1),
    }
}

fn parse_float<F>(lookup: &F, name: &str, default: f64, minimum: f64) -> f64
where
    F: Fn(&str) -> Option<String>,
{
    let raw = lookup(name).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }
    match raw.parse::<f64>() {
        Ok(value) if value >= minimum => value,
        Ok(_) => {
            warn!(
                setting = name,
                raw, minimum, "setting below minimum; using default"
            );
            default
        }
        Err(_) => {
            warn!(
                setting = name,
                raw, default, "invalid setting; using default"
            );
            default
        }
    }
}

fn parse_int<F>(lookup: &F, name: &str, default: i64, minimum: i64) -> i64
where
    F: Fn(&str) -> Option<String>,
{
    let raw = lookup(name).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }
    match raw.parse::<i64>() {
        Ok(value) if value >= minimum => value,
        Ok(_) => {
            warn!(
                setting = name,
                raw, minimum, "setting below minimum; using default"
            );
            default
        }
        Err(_) => {
            warn!(
                setting = name,
                raw, default, "invalid setting; using default"
            );
            default
        }
    }
}

fn parse_bool<F>(lookup: &F, name: &str, default: bool) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    let raw = lookup(name).unwrap_or_default();
    let raw = raw.trim().to_lowercase();
    match raw.as_str() {
        "" => default,
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => {
            warn!(
                setting = name,
                raw, default, "invalid setting; using default"
            );
            default
        }
    }
}

/// Parse `AGENT_MODELS="researcher=qwen2.5:7b,writer=qwen2.5:14b"` once.
///
/// A malformed pair or a key without an `agents/<name>.md` prompt is skipped so
/// one typo cannot break a run; that agent keeps the `MODEL` fallback.
pub fn parse_agent_models(raw: &str, known: &HashSet<String>) -> BTreeMap<String, String> {
    let mut models = BTreeMap::new();
    let raw = raw.trim();
    if raw.is_empty() {
        return models;
    }
    for pair in raw.split(',') {
        let pair = pair.trim();
        let Some((agent, model)) = pair.split_once('=') else {
            warn!(entry = pair, "invalid AGENT_MODELS entry; skipping");
            continue;
        };
        let (agent, model) = (agent.trim(), model.trim());
        if agent.is_empty() || model.is_empty() {
            warn!(entry = pair, "invalid AGENT_MODELS entry; skipping");
            continue;
        }
        if !known.contains(agent) {
            warn!(agent, "unknown AGENT_MODELS agent; skipping");
            continue;
        }
        models.insert(agent.to_string(), model.to_string());
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_from(pairs: &[(&str, &str)], known: &HashSet<String>) -> Settings {
        from_lookup(
            |name| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            },
            known,
        )
    }

    fn no_agents() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn documented_defaults_are_used_when_nothing_is_set() {
        let loaded = settings_from(&[], &no_agents());

        assert_eq!(loaded.llm_timeout_seconds, 30.0);
        assert_eq!(loaded.llm_max_retries, 2);
        assert_eq!(loaded.tool_timeout_seconds, 15.0);
        assert_eq!(loaded.run_timeout_seconds, 180.0);
        assert_eq!(loaded.max_agents, 7);
        assert!(loaded.require_approval);
        assert!(!loaded.auto_approve);
        assert_eq!(loaded.pending_ttl_hours, 24.0);
        assert_eq!(loaded.max_input_chars, 8000);
        assert!(loaded.agent_models.is_empty());
        assert_eq!(loaded.max_tool_turns, 3);
        assert_eq!(loaded.max_result_chars, 6000);
        assert_eq!(loaded.max_result_items, 10);
        assert_eq!(loaded.max_args_chars, 500);
    }

    #[test]
    fn every_documented_key_is_loaded_from_the_environment() {
        let loaded = settings_from(
            &[
                ("LLM_TIMEOUT_SECONDS", "12.5"),
                ("LLM_MAX_RETRIES", "5"),
                ("TOOL_TIMEOUT_SECONDS", "7.5"),
                ("RUN_TIMEOUT_SECONDS", "90"),
                ("MAX_AGENTS", "3"),
                ("REQUIRE_APPROVAL", "false"),
                ("AUTO_APPROVE", "true"),
                ("PENDING_TTL_HOURS", "2.5"),
                ("MAX_INPUT_CHARS", "100"),
                ("MAX_TOOL_TURNS", "5"),
                ("MAX_RESULT_CHARS", "1234"),
                ("MAX_RESULT_ITEMS", "4"),
                ("MAX_ARGS_CHARS", "99"),
            ],
            &no_agents(),
        );

        assert_eq!(loaded.llm_timeout_seconds, 12.5);
        assert_eq!(loaded.llm_max_retries, 5);
        assert_eq!(loaded.tool_timeout_seconds, 7.5);
        assert_eq!(loaded.run_timeout_seconds, 90.0);
        assert_eq!(loaded.max_agents, 3);
        assert!(!loaded.require_approval);
        assert!(loaded.auto_approve);
        assert_eq!(loaded.pending_ttl_hours, 2.5);
        assert_eq!(loaded.max_input_chars, 100);
        assert_eq!(loaded.max_tool_turns, 5);
        assert_eq!(loaded.max_result_chars, 1234);
        assert_eq!(loaded.max_result_items, 4);
        assert_eq!(loaded.max_args_chars, 99);
    }

    #[test]
    fn invalid_and_below_minimum_numbers_fall_back_to_defaults() {
        let loaded = settings_from(
            &[
                ("LLM_TIMEOUT_SECONDS", "soon"),
                ("LLM_MAX_RETRIES", "-1"),
                ("TOOL_TIMEOUT_SECONDS", "0"),
                ("RUN_TIMEOUT_SECONDS", "0.0001"),
                ("MAX_AGENTS", "lots"),
                ("MAX_INPUT_CHARS", "0"),
                ("MAX_TOOL_TURNS", "0"),
                ("MAX_RESULT_CHARS", "0"),
                ("MAX_RESULT_ITEMS", "0"),
                ("MAX_ARGS_CHARS", "0"),
                ("PENDING_TTL_HOURS", "-2"),
            ],
            &no_agents(),
        );

        assert_eq!(loaded.llm_timeout_seconds, 30.0);
        assert_eq!(loaded.llm_max_retries, 2);
        assert_eq!(loaded.tool_timeout_seconds, 15.0);
        assert_eq!(loaded.run_timeout_seconds, 180.0);
        assert_eq!(loaded.max_agents, 7);
        assert_eq!(loaded.max_input_chars, 8000);
        assert_eq!(loaded.max_tool_turns, 3);
        assert_eq!(loaded.max_result_chars, 6000);
        assert_eq!(loaded.max_result_items, 10);
        assert_eq!(loaded.max_args_chars, 500);
        assert_eq!(loaded.pending_ttl_hours, 24.0);
    }

    #[test]
    fn booleans_accept_the_same_truthy_spellings() {
        for spelling in ["1", "true", "yes", "on", "TRUE", " Yes "] {
            assert!(
                settings_from(&[("REQUIRE_APPROVAL", spelling)], &no_agents()).require_approval,
                "{spelling:?} should be true"
            );
            assert!(
                settings_from(&[("AUTO_APPROVE", spelling)], &no_agents()).auto_approve,
                "{spelling:?} should be true"
            );
        }
        for spelling in ["0", "false", "no", "off", "FALSE", " No "] {
            assert!(
                !settings_from(&[("REQUIRE_APPROVAL", spelling)], &no_agents()).require_approval,
                "{spelling:?} should be false"
            );
        }
        assert!(settings_from(&[("REQUIRE_APPROVAL", "maybe")], &no_agents()).require_approval);
        assert!(!settings_from(&[("AUTO_APPROVE", "maybe")], &no_agents()).auto_approve);
        assert!(settings_from(&[("REQUIRE_APPROVAL", "")], &no_agents()).require_approval);
    }

    #[test]
    fn agent_model_overrides_skip_malformed_and_unknown_entries() {
        let known: HashSet<String> = ["researcher", "writer", "checker"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let models = parse_agent_models(
            " researcher = qwen2.5:7b , writer= , broken , checker=qwen2.5:14b , typo=small",
            &known,
        );

        assert_eq!(
            models.get("researcher").map(String::as_str),
            Some("qwen2.5:7b")
        );
        assert_eq!(
            models.get("checker").map(String::as_str),
            Some("qwen2.5:14b")
        );
        assert_eq!(models.len(), 2);
        assert!(!models.contains_key("writer"));
        assert!(!models.contains_key("typo"));
    }

    #[test]
    fn agent_model_overrides_only_accept_runnable_agents() {
        let known: HashSet<String> = ["researcher"].into_iter().map(str::to_string).collect();
        let loaded = settings_from(&[("AGENT_MODELS", "researcher=one,typo=two")], &known);
        assert_eq!(loaded.agent_models.len(), 1);
        assert_eq!(
            loaded.agent_models.get("researcher").map(String::as_str),
            Some("one")
        );
    }

    #[test]
    fn agent_model_override_keeps_text_after_a_second_equals_sign() {
        let known: HashSet<String> = ["researcher"].into_iter().map(str::to_string).collect();
        let models = parse_agent_models("researcher=model=variant", &known);
        assert_eq!(
            models.get("researcher").map(String::as_str),
            Some("model=variant")
        );
    }

    #[test]
    fn an_empty_override_list_means_no_overrides() {
        let known: HashSet<String> = ["researcher"].into_iter().map(str::to_string).collect();
        assert!(parse_agent_models("   ", &known).is_empty());
    }

    #[test]
    fn known_agents_come_from_prompt_files_in_the_repository() {
        let known = known_agents();
        for name in ["researcher", "writer", "checker", "sun"] {
            assert!(known.contains(name), "missing agent prompt {name}");
        }
        assert!(!known.contains("README"));
    }
}
