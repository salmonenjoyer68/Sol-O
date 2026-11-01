"""Same-conversation memory lookup used by planets through the Sun's tool loop.

Recall never crosses conversations: only runs stored with the same ``source``
key are searched, and the current run is excluded. An empty or ``unknown``
source returns nothing rather than widening the query, so one customer's
inquiry can never surface in another customer's answer.
"""

from __future__ import annotations

import re
from typing import Any

from app import logger as run_store

MAX_TASK_CHARS = 300
MAX_DRAFT_CHARS = 600
TERM_RE = re.compile(r"[a-z0-9]+")


def _terms(query: str) -> list[str]:
    return [term for term in TERM_RE.findall((query or "").lower()) if len(term) >= 3]


def _clip(text: str, limit: int) -> str:
    collapsed = " ".join(str(text).split())
    return collapsed if len(collapsed) <= limit else collapsed[: limit - 1] + "…"


def recall(query: str, *, source: str, run_id: str, limit: int = 5) -> list[dict[str, Any]]:
    """Earlier finished runs from the same conversation that mention the query."""
    if not source or source == "unknown":
        return []
    rows = run_store.recall_runs(
        source=source,
        exclude_run_id=run_id,
        terms=_terms(query),
        limit=max(1, int(limit)),
    )
    return [
        {
            "run_id": row["id"],
            "created_at": row["created_at"],
            "task": _clip(row.get("task", ""), MAX_TASK_CHARS),
            "final_draft": _clip(row.get("final_draft", ""), MAX_DRAFT_CHARS),
        }
        for row in rows
    ]
