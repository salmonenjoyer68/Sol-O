//! Unit-level tests for the planet-requestable kb_search, fetch_url and recall
//! tools: fixture corpus chunking and ranking, fetch refusals, recall scoping,
//! the rendered tool sections and the audited-handler-failure path.
//!
//! Calls the handlers directly (no spawned server), so it does not exercise the
//! HTTP seam.

mod support;

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{json, Value};
use support::ROOT;
use tempfile::TempDir;

use sol_o::config::Settings;
use sol_o::fetch::fetch_url;
use sol_o::kb::search_kb_in;
use sol_o::persistence::Db;
use sol_o::recall::recall;
use sol_o::reliability::Deadline;
use sol_o::schemas::ToolRequest;
use sol_o::toolbox::{has_toolset, Toolset};

fn fixture_corpus() -> PathBuf {
    Path::new(ROOT).join("rust-tests/fixtures/kb")
}

fn repeat_word(word: &str, times: usize) -> String {
    vec![word; times].join(" ")
}

fn texts(results: &[Value]) -> Vec<String> {
    results
        .iter()
        .map(|chunk| chunk["text"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_document_is_split_into_headline_chunks_of_at_most_800_chars() {
    let results = search_kb_in(&fixture_corpus(), "refunds", 5).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["path"], "kb/guide.md");
    assert_eq!(results[0]["heading"], "Refunds");
    assert_eq!(results[0]["score"], 2);
    assert_eq!(
        results[0]["text"],
        "## Refunds\n\nRefunds take five business days and appear on the original card."
    );
}

#[test]
fn a_heading_starts_a_new_chunk_and_keeps_its_text() {
    let results = search_kb_in(&fixture_corpus(), "delivery", 5).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["heading"], "Delivery windows");
    assert_eq!(
        results[0]["text"],
        "# Delivery windows\n\nOrders placed before 14:00 ship the same day. Weekend orders ship Monday."
    );
}

#[test]
fn ranking_prefers_heading_hits_then_body_hits_then_path_and_position() {
    let tied = search_kb_in(&fixture_corpus(), "refunds delivery", 5).unwrap();
    assert_eq!(texts(&tied).len(), 2);
    assert_eq!(tied[0]["heading"], "Delivery windows");
    assert_eq!(tied[1]["heading"], "Refunds");
    assert_eq!(tied[0]["score"], 2);
    assert_eq!(tied[1]["score"], 2);

    let weighted = search_kb_in(&fixture_corpus(), "refunds card", 5).unwrap();
    assert_eq!(weighted.len(), 1);
    assert_eq!(weighted[0]["score"], 3, "heading hit plus body hit");

    let body_only = search_kb_in(&fixture_corpus(), "recyclable", 5).unwrap();
    assert_eq!(body_only.len(), 1);
    assert_eq!(body_only[0]["heading"], "Packaging");
    assert_eq!(body_only[0]["score"], 1);
}

#[test]
fn an_oversized_block_is_flushed_and_hard_sliced_keeping_its_heading() {
    let results = search_kb_in(&fixture_corpus(), "gamma", 5).unwrap();
    let gamma = repeat_word("gamma", 150);

    assert_eq!(results.len(), 2);
    for chunk in &results {
        assert_eq!(chunk["path"], "kb/notes.txt");
        assert_eq!(chunk["heading"], "");
        assert_eq!(chunk["score"], 1);
    }
    let first = results[0]["text"].as_str().unwrap();
    let second = results[1]["text"].as_str().unwrap();
    assert_eq!(first.chars().count(), 800);
    assert_eq!(second.chars().count(), 99);
    assert_eq!(format!("{first}{second}"), gamma);
}

#[test]
fn a_block_that_would_overflow_800_chars_starts_the_next_chunk() {
    let alpha = search_kb_in(&fixture_corpus(), "alpha", 5).unwrap();
    let beta = search_kb_in(&fixture_corpus(), "beta", 5).unwrap();

    assert_eq!(texts(&alpha), vec![repeat_word("alpha", 100)]);
    assert_eq!(texts(&beta), vec![repeat_word("beta", 60)]);
}

#[test]
fn terms_are_lowercase_alphanumeric_runs_of_three_plus_chars_without_stopwords() {
    let dir = fixture_corpus();
    assert!(search_kb_in(&dir, "the and for", 5).unwrap().is_empty());
    assert!(search_kb_in(&dir, "re", 5).unwrap().is_empty());
    assert!(search_kb_in(&dir, "", 5).unwrap().is_empty());
}

#[test]
fn the_limit_returns_the_top_matches_and_never_zero() {
    let dir = fixture_corpus();
    assert_eq!(search_kb_in(&dir, "gamma", 1).unwrap().len(), 1);
    assert_eq!(search_kb_in(&dir, "gamma", 0).unwrap().len(), 1);
    assert_eq!(search_kb_in(&dir, "gamma", -3).unwrap().len(), 1);
}

#[test]
fn fetch_refuses_every_non_public_address_class_and_unsupported_scheme() {
    let settings = Settings::default();
    let cases = [
        (
            "ftp://example.com/page",
            "url rejected: unsupported scheme 'ftp'",
        ),
        (
            "file:///etc/passwd",
            "url rejected: unsupported scheme 'file'",
        ),
        (
            "javascript:alert(1)",
            "url rejected: unsupported scheme 'javascript'",
        ),
        (
            "http://127.0.0.1/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://127.9.9.9/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://10.1.2.3/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://172.16.0.1/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://192.168.0.1/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://169.254.169.254/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://100.64.0.1/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://0.0.0.0/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://224.0.0.1/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://240.0.0.1/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://192.0.2.1/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://[::1]/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://[fe80::1]/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://[fc00::1]/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://[ff02::1]/",
            "url rejected: url is not publicly routable",
        ),
        ("http://[::]/", "url rejected: url is not publicly routable"),
        (
            "http://[2001:db8::1]/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://[::ffff:127.0.0.1]/",
            "url rejected: url is not publicly routable",
        ),
        (
            "http://user:pass@93.184.216.34/",
            "url rejected: credentials in url",
        ),
    ];

    for (url, expected) in cases {
        let error = fetch_url(url, &settings).expect_err(url);
        assert_eq!(error, expected, "for {url}");
    }

    assert_eq!(fetch_url("   ", &settings).unwrap_err(), "empty url");
}

#[test]
fn recall_only_returns_finished_runs_from_the_same_conversation() {
    let dir = TempDir::new().unwrap();
    let db = Db::new(dir.path().join("sol-o.db"));
    let task = "Weekend delivery plan for the cafe";
    let seed = |run_id: &str, source: &str, status: &str, draft: &str| {
        db.start_run(run_id, task, source).unwrap();
        db.save_run(
            run_id,
            task,
            status,
            draft,
            Some("ok"),
            &json!([]),
            0.5,
            None,
        )
        .unwrap();
    };
    seed(
        "run-a",
        "tg:1",
        "complete",
        "Draft mentioning weekend delivery.",
    );
    seed(
        "run-b",
        "tg:1",
        "partial",
        "Another weekend delivery draft.",
    );
    seed(
        "run-c",
        "tg:1",
        "failed",
        "A failed weekend delivery draft.",
    );
    seed("run-d", "tg:2", "complete", "A different conversation.");

    let results = recall(&db, "weekend delivery", "tg:1", "run-current", 5).unwrap();
    let ids: Vec<&str> = results
        .iter()
        .map(|row| row["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["run-b", "run-a"]);
    assert_eq!(results[0]["task"], task);
    assert_eq!(results[0]["final_draft"], "Another weekend delivery draft.");
    let mut keys: Vec<&str> = results[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["created_at", "final_draft", "run_id", "task"]);

    let excluding = recall(&db, "weekend delivery", "tg:1", "run-b", 5).unwrap();
    assert_eq!(excluding.len(), 1);
    assert_eq!(excluding[0]["run_id"], "run-a");
}

#[test]
fn recall_is_disabled_for_unknown_or_empty_sources() {
    let dir = TempDir::new().unwrap();
    let db = Db::new(dir.path().join("sol-o.db"));
    db.start_run("run-a", "weekend delivery", "unknown")
        .unwrap();
    db.save_run(
        "run-a",
        "weekend delivery",
        "complete",
        "",
        None,
        &json!([]),
        0.0,
        None,
    )
    .unwrap();

    assert!(recall(&db, "weekend delivery", "unknown", "run-x", 5)
        .unwrap()
        .is_empty());
    assert!(recall(&db, "weekend delivery", "", "run-x", 5)
        .unwrap()
        .is_empty());
}

#[test]
fn recall_clips_the_task_and_the_draft_and_collapses_whitespace() {
    let dir = TempDir::new().unwrap();
    let db = Db::new(dir.path().join("sol-o.db"));
    let task = format!("weekend   delivery {}", "t".repeat(400));
    let draft = format!("delivery\n\n{}", "d".repeat(700));
    db.start_run("run-a", &task, "tg:1").unwrap();
    db.save_run(
        "run-a",
        &task,
        "complete",
        &draft,
        None,
        &json!([]),
        0.0,
        None,
    )
    .unwrap();

    let results = recall(&db, "delivery", "tg:1", "run-current", 5).unwrap();
    let clipped_task = results[0]["task"].as_str().unwrap();
    let clipped_draft = results[0]["final_draft"].as_str().unwrap();
    assert_eq!(clipped_task.chars().count(), 300);
    assert!(clipped_task.starts_with("weekend delivery t"));
    assert!(clipped_task.ends_with('…'));
    assert_eq!(clipped_draft.chars().count(), 600);
    assert!(clipped_draft.starts_with("delivery d"));
    assert!(clipped_draft.ends_with('…'));
}

const KB_SEARCH: &str = "- kb_search {\"query\": str, \"limit\": int=5} — Search the company knowledge base; returns matching chunks with their file path (trimmed to the result budget; `limit` is clamped to the documented range).";
const FETCH_URL: &str = "- fetch_url {\"url\": str} — Download one http(s) page and return its visible text (trimmed to the result budget and marked `truncated` when partial).";
const CALC: &str = "- calc {\"expression\": str} — Evaluate arithmetic and date arithmetic, e.g. \"3 * 19.99\" or \"days_between('2026-11-01','2026-12-01')\".";
const RECALL: &str = "- recall {\"query\": str, \"limit\": int=5} — Search earlier runs from this same conversation (trimmed to the result budget).";

#[test]
fn every_agent_renders_exactly_the_reference_tool_list() {
    let settings = Settings::default();
    let dir = TempDir::new().unwrap();
    let db = Db::new(dir.path().join("sol-o.db"));
    let emit = |_: &str, _: Value| {};
    let expected = [
        ("researcher", vec![KB_SEARCH, FETCH_URL, RECALL]),
        ("writer", vec![KB_SEARCH, CALC, RECALL]),
        ("checker", vec![KB_SEARCH, CALC]),
    ];

    for (agent, lines) in expected {
        let toolset = Toolset::new(
            &settings,
            &db,
            &emit,
            agent,
            1,
            "run-1",
            "tg:1",
            Deadline::new(Instant::now(), 60.0),
        );
        let reference = format!(
            "Available tools (this rendered list is the only source of allowed names):\n{}\n\nRules:",
            lines.join("\n")
        );
        let contract = toolset.contract();
        assert!(
            contract.contains(&reference),
            "{agent} rendered:\n{contract}"
        );
    }

    assert!(!has_toolset("sun"));
    assert!(!has_toolset("logger"));
}

#[test]
fn a_failing_handler_is_audited_as_a_failed_call_and_never_raises() {
    let dir = TempDir::new().unwrap();
    let db = Db::new(dir.path().join("sol-o.db"));
    db.start_run("run-1", "explain weekend delivery", "tg:1")
        .unwrap();
    let settings = Settings::default();
    let emit = |_: &str, _: Value| {};
    let deadline = Deadline::new(Instant::now(), 60.0);
    let researcher = Toolset::new(
        &settings,
        &db,
        &emit,
        "researcher",
        1,
        "run-1",
        "tg:1",
        deadline,
    );
    let writer = Toolset::new(
        &settings, &db, &emit, "writer", 1, "run-1", "tg:1", deadline,
    );

    let failed_search = writer.run(&ToolRequest {
        tool: "kb_search".to_string(),
        args: json!({"query": "refund", "limit": "lots"})
            .as_object()
            .unwrap()
            .clone(),
    });
    assert_eq!(failed_search["ok"], false);
    assert!(failed_search["error"]
        .as_str()
        .unwrap()
        .starts_with("invalid limit"));

    let failed_fetch = researcher.run(&ToolRequest {
        tool: "fetch_url".to_string(),
        args: json!({"url": "  "}).as_object().unwrap().clone(),
    });
    assert_eq!(failed_fetch["ok"], false);
    assert_eq!(failed_fetch["error"], "empty url");

    let detail = db.get_run_detail("run-1").unwrap().expect("run exists");
    let rows: Vec<&sol_o::persistence::ToolCallRow> = detail
        .tool_calls
        .iter()
        .filter(|row| row.status == "failed")
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].tool, "kb_search");
    assert!(rows[0]
        .error_message
        .as_deref()
        .unwrap()
        .starts_with("invalid limit"));
    assert_eq!(rows[1].tool, "fetch_url");
    assert_eq!(rows[1].error_message.as_deref(), Some("empty url"));
}
