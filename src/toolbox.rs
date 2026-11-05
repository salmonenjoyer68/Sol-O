//! Tool registry and the Sun-side executor, mirroring `app/toolbox.py`.
//!
//! A planet asks for tools inside its JSON output; the executor checks the
//! per-agent allowlist, clamps requested limits, runs the handler, trims the
//! result to the character budget, audits every call and emits an event.
//! Adding a handler means one [`TOOLS`] entry plus the agent allowlists below.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::calc::calculate;
use crate::config::Settings;
use crate::fetch::fetch_url;
use crate::kb::search_kb;
use crate::persistence::{Db, NewToolCall};
use crate::recall::recall;
use crate::reliability::{redact, Deadline};
use crate::schemas::ToolRequest;

pub const TOOL_BUDGET_INSTRUCTION: &str = "Tool budget is exhausted. Return your normal output object now; no more tools will run.";
pub const TOOL_RESULTS_HEADER: &str = "Tool results (JSON):\n";

const CONTRACT_TEMPLATE: &str = "
## Tool envelope (runtime contract)

You may answer with your normal output object, or ask the Sun to run tools first. To ask, return exactly this object and nothing else:
{\"tool_requests\": [{\"tool\": \"<name>\", \"args\": {...}}]}

The envelope has no `status` key; it must contain only the `tool_requests` key. The Sun checks each request against the allowlist below, executes it, and calls you again with \"Tool results (JSON):\" holding one object per request with `tool`, `ok`, `truncated` and either `result` or `error`. A result marked `truncated: true` was capped at some layer (the tool itself or the result budget): treat it as partial, never as complete. The Sun executes tool requests; you only ask. Never claim to have run a tool yourself.

Available tools (this rendered list is the only source of allowed names):
{tools}

Rules: at most {turns} tool turns per step. After that you must return your normal output object. Never request a tool that is not listed above. A requested `limit` outside 1..{items} is clamped into that range before the tool runs.
";

/// Per-step context a handler may need.
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub run_id: String,
    pub source: String,
    pub agent: String,
    pub step: i64,
    pub deadline: Deadline,
    pub settings: Settings,
    pub db: Db,
}

pub type ToolHandler = fn(&Map<String, Value>, &ToolContext) -> Result<Value, String>;

/// One callable tool with the one-line signature shown to the model.
pub struct Tool {
    pub name: &'static str,
    pub signature: &'static str,
    pub description: &'static str,
    pub required: &'static [&'static str],
    pub handler: ToolHandler,
}

/// `int(args.get("limit", 5))`: a bad limit reaches the handler and fails there.
fn limit_arg(args: &Map<String, Value>) -> Result<i64, String> {
    match args.get("limit") {
        None => Ok(5),
        Some(Value::Bool(flag)) => Ok(i64::from(*flag)),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value.trunc() as i64))
            .ok_or_else(|| format!("invalid limit: {number}")),
        Some(Value::String(text)) => text
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid limit: {text}")),
        Some(other) => Err(format!("invalid limit: {other}")),
    }
}

fn search_handler(args: &Map<String, Value>, _context: &ToolContext) -> Result<Value, String> {
    let query = args.get("query").map(stringify_value).unwrap_or_default();
    Ok(Value::Array(search_kb(&query, limit_arg(args)?)?))
}

fn fetch_handler(args: &Map<String, Value>, context: &ToolContext) -> Result<Value, String> {
    let url = args.get("url").map(stringify_value).unwrap_or_default();
    fetch_url(&url, &context.settings)
}

fn calc_handler(args: &Map<String, Value>, _context: &ToolContext) -> Result<Value, String> {
    let expression = args
        .get("expression")
        .map(stringify_value)
        .unwrap_or_default();
    calculate(&expression)
}

fn recall_handler(args: &Map<String, Value>, context: &ToolContext) -> Result<Value, String> {
    let query = args.get("query").map(stringify_value).unwrap_or_default();
    Ok(Value::Array(recall(
        &context.db,
        &query,
        &context.source,
        &context.run_id,
        limit_arg(args)?,
    )?))
}

/// The registry: add an entry here, then allow it below.
pub static TOOLS: &[Tool] = &[
    Tool {
        name: "kb_search",
        signature: "{\"query\": str, \"limit\": int=5}",
        description: "Search the company knowledge base; returns matching chunks with their file path (trimmed to the result budget; `limit` is clamped to the documented range).",
        required: &["query"],
        handler: search_handler,
    },
    Tool {
        name: "fetch_url",
        signature: "{\"url\": str}",
        description: "Download one http(s) page and return its visible text (trimmed to the result budget and marked `truncated` when partial).",
        required: &["url"],
        handler: fetch_handler,
    },
    Tool {
        name: "calc",
        signature: "{\"expression\": str}",
        description: "Evaluate arithmetic and date arithmetic, e.g. \"3 * 19.99\" or \"days_between('2026-11-01','2026-12-01')\".",
        required: &["expression"],
        handler: calc_handler,
    },
    Tool {
        name: "recall",
        signature: "{\"query\": str, \"limit\": int=5}",
        description: "Search earlier runs from this same conversation (trimmed to the result budget).",
        required: &["query"],
        handler: recall_handler,
    },
];

/// The exact per-agent allowlist; only names present in [`TOOLS`] are offered.
pub fn agent_tool_names(agent: &str) -> &'static [&'static str] {
    match agent {
        "researcher" => &["kb_search", "fetch_url", "recall"],
        "writer" => &["kb_search", "calc", "recall"],
        "checker" => &["kb_search", "calc"],
        _ => &[],
    }
}

pub fn has_toolset(agent: &str) -> bool {
    matches!(agent, "researcher" | "writer" | "checker")
}

fn allowed_tools(agent: &str) -> Vec<&'static Tool> {
    agent_tool_names(agent)
        .iter()
        .filter_map(|name| TOOLS.iter().find(|tool| tool.name == *name))
        .collect()
}

/// `json.dumps(value, ensure_ascii=False, default=str)` separators.
fn python_json(value: &Value) -> String {
    let mut buffer = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut buffer, PythonFormatter);
    value
        .serialize(&mut serializer)
        .expect("JSON values always serialize");
    String::from_utf8(buffer).expect("JSON output is UTF-8")
}

struct PythonFormatter;

impl serde_json::ser::Formatter for PythonFormatter {
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        writer.write_all(b": ")
    }
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// `_fit`: trim a result until the serialized payload fits; never a placeholder.
pub fn fit(mut payload: Value, max_result_chars: usize) -> Value {
    let result = payload.get("result").cloned();
    let mut capped = payload
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if result
        .as_ref()
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        capped = true;
    }
    if result.is_none() {
        if let Some(object) = payload.as_object_mut() {
            object.insert("truncated".to_string(), Value::Bool(capped));
        }
        return payload;
    }

    let tool = payload.get("tool").cloned().unwrap_or(Value::Null);
    let ok = payload.get("ok").cloned().unwrap_or(Value::Null);
    let make = |value: Value, capped: bool| {
        let mut object = Map::new();
        object.insert("tool".to_string(), tool.clone());
        object.insert("ok".to_string(), ok.clone());
        object.insert("truncated".to_string(), Value::Bool(capped));
        object.insert("result".to_string(), value);
        Value::Object(object)
    };
    let fits = |value: &Value| char_len(&python_json(value)) <= max_result_chars;

    let mut current = result.expect("result is present");
    if fits(&make(current.clone(), capped)) {
        return make(current, capped);
    }
    capped = true;
    loop {
        let Some(shrunk) = shrink(current.clone(), &make, max_result_chars) else {
            break;
        };
        current = shrunk;
        if fits(&make(current.clone(), capped)) {
            break;
        }
    }
    if !fits(&make(current.clone(), capped)) {
        let serialized = python_json(&current);
        let clipped = clip(&serialized, |candidate| {
            fits(&make(Value::String(candidate.to_string()), capped))
        });
        current = Value::String(clipped.unwrap_or_default());
    }
    make(current, capped)
}

fn clip(text: &str, fits: impl Fn(&str) -> bool) -> Option<String> {
    if fits(text) {
        return Some(text.to_string());
    }
    let characters: Vec<char> = text.chars().collect();
    let mut best = None;
    let (mut low, mut high) = (0_usize, characters.len());
    while low <= high {
        let middle = (low + high) / 2;
        let candidate = format!("{}…", characters[..middle].iter().collect::<String>());
        if fits(&candidate) {
            best = Some(candidate);
            low = middle + 1;
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    best
}

fn longest_text_key(value: &Map<String, Value>) -> Option<String> {
    value
        .iter()
        .filter(|(_, item)| item.is_string())
        .max_by_key(|(_, item)| char_len(&python_json(item)))
        .map(|(key, _)| key.clone())
}

fn shrink(
    value: Value,
    make: &dyn Fn(Value, bool) -> Value,
    max_result_chars: usize,
) -> Option<Value> {
    let fits = |candidate: Value| char_len(&python_json(&make(candidate, true))) <= max_result_chars;
    match value {
        Value::String(text) => clip(&text, |candidate| {
            fits(Value::String(candidate.to_string()))
        })
        .map(Value::String),
        Value::Array(mut items) => {
            if items.len() > 1 {
                items.pop();
                return Some(Value::Array(items));
            }
            if items.is_empty() {
                return None;
            }
            let item = items.pop().expect("one item remains");
            shrink(item, &|candidate: Value, _capped: bool| make(Value::Array(vec![candidate]), true), max_result_chars)
                .map(|shrunk| Value::Array(vec![shrunk]))
        }
        Value::Object(mut object) => {
            let key = longest_text_key(&object)?;
            let original = object.get(&key)?.as_str()?.to_string();
            let object_for = |candidate: &str| {
                let mut next = object.clone();
                next.insert(key.clone(), Value::String(candidate.to_string()));
                make(Value::Object(next), true)
            };
            let clipped = clip(&original, |candidate| {
                char_len(&python_json(&object_for(candidate))) <= max_result_chars
            })?;
            if clipped == original {
                return None;
            }
            object.insert(key, Value::String(clipped));
            Some(Value::Object(object))
        }
        _ => None,
    }
}

fn stringify_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => python_json(other),
    }
}

/// `_clamp_limit`: clamp a requested result count into 1..`max_result_items`.
fn clamp_limit(args: Map<String, Value>, max_result_items: i64) -> Map<String, Value> {
    let Some(requested) = args.get("limit") else {
        return args;
    };
    let parsed = match requested {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.fract() == 0.0)
                .map(|value| value as i64)
        }),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    let Some(parsed) = parsed else {
        return args;
    };
    let clamped = parsed.clamp(1, max_result_items.max(1));
    if matches!(requested, Value::Number(number) if number.as_i64() == Some(clamped)) {
        return args;
    }
    let mut next = args;
    next.insert("limit".to_string(), Value::from(clamped));
    next
}

/// The tools one agent step may use, plus the audit and event sinks.
pub struct Toolset<'a> {
    pub settings: &'a Settings,
    pub db: &'a Db,
    pub emit: &'a dyn Fn(&str, Value),
    pub agent: String,
    pub step: i64,
    pub run_id: String,
    pub source: String,
    pub deadline: Deadline,
    pub tools: Vec<&'static Tool>,
}

impl<'a> Toolset<'a> {
    pub fn new(
        settings: &'a Settings,
        db: &'a Db,
        emit: &'a dyn Fn(&str, Value),
        agent: &str,
        step: i64,
        run_id: &str,
        source: &str,
        deadline: Deadline,
    ) -> Self {
        Self {
            settings,
            db,
            emit,
            agent: agent.to_string(),
            step,
            run_id: run_id.to_string(),
            source: source.to_string(),
            deadline,
            tools: allowed_tools(agent),
        }
    }

    pub fn contract(&self) -> String {
        let tools = self
            .tools
            .iter()
            .map(|tool| format!("- {} {} — {}", tool.name, tool.signature, tool.description))
            .collect::<Vec<_>>()
            .join("\n");
        CONTRACT_TEMPLATE
            .replace("{tools}", &tools)
            .replace("{turns}", &self.settings.max_tool_turns.to_string())
            .replace("{items}", &self.settings.max_result_items.to_string())
    }

    fn audit(&self, call: NewToolCall) {
        if let Err(error) = self.db.save_tool_call(&call) {
            tracing::error!(tool = call.tool, %error, "could not write tool_calls row");
        }
    }

    /// Execute one request; never raises, always returns a result object.
    pub fn run(&self, request: &ToolRequest) -> Value {
        let name = request.tool.trim().to_string();
        let started = std::time::Instant::now();
        let started_at = crate::persistence::now_iso();
        let args = clamp_limit(request.args.clone(), self.settings.max_result_items);
        (self.emit)(
            "tool_requested",
            serde_json::json!({"step": self.step, "agent": self.agent, "tool": name}),
        );

        let outcome = self.execute(&name, &args);
        let error = outcome.get("error").and_then(Value::as_str).map(str::to_string);
        let duration_ms = started.elapsed().as_millis() as i64;
        let args_text: String = python_json(&Value::Object(args))
            .chars()
            .take(self.settings.max_args_chars.max(0) as usize)
            .collect();
        self.audit(NewToolCall {
            run_id: self.run_id.clone(),
            tool: name.clone(),
            started_at,
            duration_ms,
            status: if error.is_some() { "failed" } else { "ok" }.to_string(),
            error_message: error,
            args: args_text,
        });
        let payload = fit(outcome, self.settings.max_result_chars.max(0) as usize);
        let result = payload
            .get("result")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        (self.emit)(
            "tool_done",
            serde_json::json!({
                "step": self.step,
                "agent": self.agent,
                "tool": name,
                "ok": payload.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "ms": duration_ms,
                "truncated": payload.get("truncated").and_then(Value::as_bool).unwrap_or(false),
                "chars": char_len(&python_json(&result)),
            }),
        );
        payload
    }

    fn execute(&self, name: &str, args: &Map<String, Value>) -> Value {
        let failure = |error: String| {
            serde_json::json!({"tool": name, "ok": false, "error": error})
        };
        let Some(tool) = self.tools.iter().find(|tool| tool.name == name) else {
            return failure(format!("tool not allowed: {name}"));
        };
        if self.deadline.expired() {
            return failure("run_out_of_time".to_string());
        }
        let missing: Vec<&str> = tool
            .required
            .iter()
            .copied()
            .filter(|key| !args.contains_key(*key))
            .collect();
        if !missing.is_empty() {
            return failure(format!("missing argument(s): {}", missing.join(", ")));
        }
        let context = ToolContext {
            run_id: self.run_id.clone(),
            source: self.source.clone(),
            agent: self.agent.clone(),
            step: self.step,
            deadline: self.deadline.clone(),
            settings: self.settings.clone(),
            db: self.db.clone(),
        };
        match (tool.handler)(args, &context) {
            Ok(result) => serde_json::json!({"tool": name, "ok": true, "result": result}),
            Err(message) => {
                let message: String = redact(&message).chars().take(300).collect();
                let message = if message.is_empty() {
                    "tool error".to_string()
                } else {
                    message
                };
                failure(message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(tool: &str, ok: bool, result: Value) -> Value {
        json!({"tool": tool, "ok": ok, "result": result})
    }

    #[test]
    fn a_small_result_is_untouched_and_not_truncated() {
        let payload = fit(envelope("calc", true, json!({"value": "2"})), 200);
        assert_eq!(payload["truncated"], false);
        assert_eq!(payload["result"]["value"], "2");
    }

    #[test]
    fn an_oversized_string_is_clipped_not_discarded() {
        let text = format!("head-{}", "x".repeat(500));
        let payload = fit(envelope("calc", true, json!({"value": text})), 200);
        assert_eq!(payload["truncated"], true);
        let value = payload["result"]["value"].as_str().unwrap();
        assert!(value.starts_with("head-"));
        assert!(value.ends_with('…'));
        assert!(char_len(&python_json(&payload)) <= 200);
    }

    #[test]
    fn an_oversized_list_drops_the_tail_then_clips_the_last_item() {
        let items: Vec<Value> = (0..10)
            .map(|index| json!({"path": "kb/delivery.md", "text": format!("item-{index} {}", "y".repeat(80))}))
            .collect();
        let payload = fit(envelope("kb_search", true, Value::Array(items.clone())), 320);
        assert_eq!(payload["truncated"], true);
        let kept = payload["result"].as_array().unwrap();
        assert!(kept.len() < items.len());
        assert_eq!(kept[0], items[0]);
        assert!(char_len(&python_json(&payload)) <= 320);

        let single = fit(envelope("kb_search", true, json!(["first item ".to_string() + &"z".repeat(800)])), 220);
        assert_eq!(single["truncated"], true);
        let clipped = single["result"][0].as_str().unwrap();
        assert!(clipped.starts_with("first item"));
        assert!(clipped.ends_with('…'));
        assert!(char_len(&python_json(&single)) <= 220);
    }

    #[test]
    fn an_oversized_dict_clips_its_longest_text_field() {
        let result = json!({
            "url": "https://example.com/delivery",
            "status": 200,
            "text": "page text ".repeat(200),
            "note": "short",
        });
        let payload = fit(envelope("fetch_url", true, result.clone()), 400);
        assert_eq!(payload["truncated"], true);
        assert_eq!(payload["result"]["url"], result["url"]);
        assert_eq!(payload["result"]["status"], 200);
        assert_eq!(payload["result"]["note"], "short");
        assert!(payload["result"]["text"].as_str().unwrap().starts_with("page text "));
        assert!(char_len(&python_json(&payload)) <= 400);
    }

    #[test]
    fn a_tool_that_reported_its_own_truncation_stays_truncated() {
        let payload = fit(
            json!({"tool": "fetch_url", "ok": true, "result": {"url": "u", "text": "t", "truncated": true}}),
            4000,
        );
        assert_eq!(payload["truncated"], true);
        assert_eq!(payload["result"]["truncated"], true);
    }

    #[test]
    fn a_cap_smaller_than_the_envelope_keeps_the_audit_fields() {
        let payload = fit(envelope("calc", true, json!("x".repeat(500))), 20);
        assert_eq!(payload["tool"], "calc");
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["truncated"], true);
        assert_eq!(payload["result"], "");
        assert!(char_len(&python_json(&payload)) > 20);
    }

    #[test]
    fn a_failed_call_keeps_its_fields_and_is_not_trimmed() {
        let payload = fit(json!({"tool": "kb_search", "ok": false, "error": "tool not allowed: x"}), 10);
        assert_eq!(payload["truncated"], false);
        assert!(payload["error"].as_str().unwrap().contains("tool not allowed"));
    }

    #[test]
    fn limits_are_clamped_and_bad_values_reach_the_handler() {
        let args = clamp_limit(json!({"limit": 10_000}).as_object().unwrap().clone(), 4);
        assert_eq!(args["limit"], 4);
        let args = clamp_limit(json!({"limit": 0}).as_object().unwrap().clone(), 10);
        assert_eq!(args["limit"], 1);
        let args = clamp_limit(json!({"limit": "5"}).as_object().unwrap().clone(), 10);
        assert_eq!(args["limit"], 5);
        let args = clamp_limit(json!({"limit": "x"}).as_object().unwrap().clone(), 10);
        assert_eq!(args["limit"], "x");
        let args = clamp_limit(json!({}).as_object().unwrap().clone(), 10);
        assert!(args.is_empty());
    }

    #[test]
    fn the_contract_lists_only_registered_allowed_tools() {
        let settings = Settings::default();
        let db = Db::new(":memory:");
        let emit = |_: &str, _: Value| {};
        let writer = Toolset::new(&settings, &db, &emit, "writer", 1, "run", "src", Deadline::new(std::time::Instant::now(), 60.0));
        assert!(writer.contract().contains("- calc {\"expression\": str} — "));
        assert!(writer.contract().contains("- kb_search {"));
        assert!(!writer.contract().contains("- fetch_url {"));
        assert!(writer.contract().contains("at most 3 tool turns"));
        assert!(writer.contract().contains("1..10"));
        let researcher = Toolset::new(&settings, &db, &emit, "researcher", 1, "run", "src", Deadline::new(std::time::Instant::now(), 60.0));
        assert_eq!(researcher.tools.len(), 3);
        assert!(!researcher.contract().contains("- calc {"));
        assert!(has_toolset("logger") == false);
    }
}
