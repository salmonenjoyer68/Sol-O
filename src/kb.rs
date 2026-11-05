//! Knowledge-base search over `kb/**/*.md` and `kb/**/*.txt`, mirroring `tools/kb.py`.
//!
//! Files are chunked on blank-line blocks at most [`CHUNK_CHARS`] long, each
//! chunk remembering its file path and the last `#` heading above it, then
//! ranked by query-term overlap: a heading hit scores two, a body hit one.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::ROOT;

pub const CHUNK_CHARS: usize = 800;

const STOPWORDS: [&str; 12] = [
    "the", "and", "for", "with", "that", "this", "from", "have", "are", "was", "you", "your",
];

#[derive(Debug, Clone)]
struct Chunk {
    heading: String,
    text: String,
    shown: String,
}

/// Search the repository's `kb/` directory.
pub fn search_kb(query: &str, limit: i64) -> Result<Vec<Value>, String> {
    search_kb_in(&Path::new(ROOT).join("kb"), query, limit)
}

/// Search one corpus directory; `shown` paths are relative to its parent.
pub fn search_kb_in(dir: &Path, query: &str, limit: i64) -> Result<Vec<Value>, String> {
    let terms = terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let chunks = load(dir);
    let mut scored: Vec<(i64, usize, &str, &Chunk)> = Vec::new();
    for (position, chunk) in chunks.iter().enumerate() {
        let heading = chunk.heading.to_lowercase();
        let body = chunk.text.to_lowercase();
        let mut score = 0;
        for term in &terms {
            if heading.contains(term.as_str()) {
                score += 2;
            } else if body.contains(term.as_str()) {
                score += 1;
            }
        }
        if score > 0 {
            scored.push((score, position, chunk.shown.as_str(), chunk));
        }
    }
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then(left.2.cmp(right.2))
            .then(left.1.cmp(&right.1))
    });
    scored.truncate(limit.max(1) as usize);
    Ok(scored
        .into_iter()
        .map(|(score, _, _, chunk)| {
            json!({
                "path": chunk.shown,
                "heading": chunk.heading,
                "text": chunk.text,
                "score": score,
            })
        })
        .collect())
}

/// Read every markdown/text file under `dir`, sorted by path, as chunks.
fn load(dir: &Path) -> Vec<Chunk> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    collect(dir, &mut files);
    files.sort();

    let mut chunks = Vec::new();
    for path in files {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(%error, file = %path.display(), "could not read knowledge file");
                continue;
            }
        };
        let shown = dir
            .parent()
            .filter(|parent| path.starts_with(parent))
            .map(|parent| path.strip_prefix(parent).unwrap_or(&path))
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        for (heading, text) in chunk(&text) {
            chunks.push(Chunk {
                heading,
                text,
                shown: shown.clone(),
            });
        }
    }
    chunks
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("md" | "txt")
        ) {
            files.push(path);
        }
    }
}

/// Split one document into <=[`CHUNK_CHARS`] chunks that remember their heading.
fn chunk(text: &str) -> Vec<(String, String)> {
    let mut heading = String::new();
    let mut chunks: Vec<(String, String)> = Vec::new();
    let mut buffer = String::new();
    for block in blocks(text) {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        if block.starts_with('#') {
            if !buffer.is_empty() {
                chunks.push((heading.clone(), std::mem::take(&mut buffer)));
            }
            heading = block.trim_start_matches('#').trim().to_string();
            buffer = block.to_string();
            continue;
        }
        let candidate = if buffer.is_empty() {
            block.to_string()
        } else {
            format!("{buffer}\n\n{block}")
        };
        if !buffer.is_empty() && candidate.chars().count() > CHUNK_CHARS {
            chunks.push((heading.clone(), std::mem::take(&mut buffer)));
            buffer = block.to_string();
        } else {
            buffer = candidate;
        }
    }
    if !buffer.is_empty() {
        chunks.push((heading, buffer));
    }

    let mut sized = Vec::new();
    for (heading, body) in chunks {
        let characters: Vec<char> = body.chars().collect();
        for start in (0..characters.len()).step_by(CHUNK_CHARS) {
            let end = (start + CHUNK_CHARS).min(characters.len());
            sized.push((heading.clone(), characters[start..end].iter().collect()));
        }
    }
    sized
}

/// `re.split(r"\n\s*\n", text)`: blank-line blocks.
fn blocks(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut blocks = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '\n' {
            let mut end = index;
            while end < characters.len() && characters[end].is_whitespace() {
                end += 1;
            }
            if let Some(last) = (index..end).rev().find(|at| characters[*at] == '\n') {
                if last > index {
                    blocks.push(characters[start..index].iter().collect());
                    start = last + 1;
                    index = last + 1;
                    continue;
                }
            }
        }
        index += 1;
    }
    blocks.push(characters[start..].iter().collect());
    blocks
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
    if term.chars().count() >= 3 && !STOPWORDS.contains(&term.as_str()) {
        terms.push(term);
    }
}
