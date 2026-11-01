"""Knowledge-base lookup used by planets through the Sun's tool loop.

Reads ``kb/**/*.md`` and ``kb/**/*.txt`` and ranks chunks by query-term overlap.
There are no embeddings and no external service: the knowledge base is a
directory the operator edits. The parsed index is cached against file mtimes.
"""

from __future__ import annotations

import logging
import re
from pathlib import Path
from typing import Any

logger = logging.getLogger("sol_o.tools.kb")

KB_DIR = Path(__file__).resolve().parent.parent / "kb"
CHUNK_CHARS = 800
BLOCK_RE = re.compile(r"\n\s*\n")
TOKEN_RE = re.compile(r"[a-z0-9]+")

STOPWORDS = frozenset(
    {
        "the",
        "and",
        "for",
        "with",
        "that",
        "this",
        "from",
        "have",
        "are",
        "was",
        "you",
        "your",
    }
)

_index: tuple[tuple[tuple[str, int, int], ...], list[dict[str, Any]]] | None = None


def _chunk(text: str) -> list[dict[str, Any]]:
    """Split one document into <=CHUNK_CHARS chunks that remember their heading."""
    heading = ""
    chunks: list[dict[str, Any]] = []
    buffer = ""
    for block in BLOCK_RE.split(text):
        block = block.strip()
        if not block:
            continue
        if block.startswith("#"):
            if buffer:
                chunks.append({"heading": heading, "text": buffer})
                buffer = ""
            heading = block.lstrip("#").strip()
            buffer = block
            continue
        candidate = f"{buffer}\n\n{block}" if buffer else block
        if buffer and len(candidate) > CHUNK_CHARS:
            chunks.append({"heading": heading, "text": buffer})
            buffer = block
        else:
            buffer = candidate
    if buffer:
        chunks.append({"heading": heading, "text": buffer})

    sized: list[dict[str, Any]] = []
    for chunk in chunks:
        body = chunk["text"]
        for start in range(0, len(body), CHUNK_CHARS):
            sized.append({**chunk, "text": body[start : start + CHUNK_CHARS]})
    return sized


def _load() -> list[dict[str, Any]]:
    """All chunks for the current ``kb/`` contents, cached by mtime and size."""
    global _index
    if not KB_DIR.exists():
        return []
    files = sorted([*KB_DIR.rglob("*.md"), *KB_DIR.rglob("*.txt")])
    fingerprint: list[tuple[str, int, int]] = []
    for path in files:
        try:
            stat = path.stat()
        except OSError:
            continue
        fingerprint.append((str(path), stat.st_mtime_ns, stat.st_size))
    key = tuple(fingerprint)
    if _index is not None and _index[0] == key:
        return _index[1]

    chunks: list[dict[str, Any]] = []
    for path in files:
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            logger.warning("could not read knowledge file %s", path)
            continue
        except UnicodeDecodeError:
            logger.warning("knowledge file is not utf-8: %s", path)
            continue
        try:
            shown = str(path.relative_to(KB_DIR.parent))
        except ValueError:  # KB_DIR overridden to a directory outside the repo
            shown = str(path)
        for chunk in _chunk(text):
            chunks.append({**chunk, "shown": shown})
    _index = (key, chunks)
    return chunks


def _terms(query: str) -> list[str]:
    return [
        term
        for term in TOKEN_RE.findall(query.lower())
        if len(term) >= 3 and term not in STOPWORDS
    ]


def search_kb(query: str, limit: int = 5) -> list[dict[str, Any]]:
    """Return the best matching chunks as ``{path, heading, text, score}``."""
    terms = _terms(query)
    if not terms:
        return []

    scored: list[tuple[int, int, str, dict[str, Any]]] = []
    for position, chunk in enumerate(_load()):
        heading = chunk["heading"].lower()
        body = chunk["text"].lower()
        score = 0
        for term in terms:
            if term in heading:
                score += 2
            elif term in body:
                score += 1
        if score:
            scored.append((score, position, chunk["shown"], chunk))

    scored.sort(key=lambda item: (-item[0], item[2], item[1]))
    results: list[dict[str, Any]] = []
    for score, _position, shown, chunk in scored[: max(1, int(limit))]:
        results.append(
            {
                "path": shown,
                "heading": chunk["heading"],
                "text": chunk["text"],
                "score": score,
            }
        )
    return results
