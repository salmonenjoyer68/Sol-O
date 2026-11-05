//! Same-conversation memory lookup, mirroring `tools/recall.py`.
//!
//! Recall never crosses conversations: only runs stored with the same `source`
//! key are searched, and the current run is excluded. An empty or `unknown`
//! source returns nothing rather than widening the query.

use serde_json::{json, Value};

use crate::persistence::Db;

pub const MAX_TASK_CHARS: usize = 300;
pub const MAX_DRAFT_CHARS: usize = 600;

/// Earlier finished runs from the same conversation that mention the query.
pub fn recall(
    db: &Db,
    query: &str,
    source: &str,
    run_id: &str,
    limit: i64,
) -> Result<Vec<Value>, String> {
    if source.is_empty() || source == "unknown" {
        return Ok(Vec::new());
    }
    let rows = db
        .recall_runs(source, run_id, &terms(query), limit.max(1))
        .map_err(|error| error.to_string())?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "run_id": row.id,
                "created_at": row.created_at,
                "task": clip(&row.task, MAX_TASK_CHARS),
                "final_draft": clip(&row.final_draft, MAX_DRAFT_CHARS),
            })
        })
        .collect())
}

fn terms(query: &str) -> Vec<String> {
    let lowered = query.to_lowercase();
    let mut terms: Vec<String> = Vec::new();
    let mut current = String::new();
    for character in lowered.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            current.push(character);
        } else if !current.is_empty() {
            push_term(&mut terms, std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        push_term(&mut terms, current);
    }
    terms
}

fn push_term(terms: &mut Vec<String>, term: String) {
    if term.chars().count() >= 3 {
        terms.push(term);
    }
}

fn clip(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let mut clipped: String = collapsed.chars().take(limit - 1).collect();
    clipped.push('…');
    clipped
}
