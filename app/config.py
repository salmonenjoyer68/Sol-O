"""Runtime settings loaded once from .env (Phase 5).

All limits and safety switches live here so no module hardcodes a timeout,
retry count or agent cap.
"""

from __future__ import annotations

import logging
import os
from dataclasses import dataclass
from pathlib import Path

from dotenv import load_dotenv

ROOT = Path(__file__).resolve().parent.parent
load_dotenv(ROOT / ".env")

logger = logging.getLogger("sol_o.config")


def _float(name: str, default: float, minimum: float = 0.0) -> float:
    raw = os.getenv(name, "").strip()
    if not raw:
        return default
    try:
        value = float(raw)
    except ValueError:
        logger.warning("invalid %s=%r; using default %s", name, raw, default)
        return default
    if value < minimum:
        logger.warning("%s below minimum %s; using default %s", name, minimum, default)
        return default
    return value


def _int(name: str, default: int, minimum: int = 0) -> int:
    raw = os.getenv(name, "").strip()
    if not raw:
        return default
    try:
        value = int(raw)
    except ValueError:
        logger.warning("invalid %s=%r; using default %s", name, raw, default)
        return default
    if value < minimum:
        logger.warning("%s below minimum %s; using default %s", name, minimum, default)
        return default
    return value


def _bool(name: str, default: bool) -> bool:
    raw = os.getenv(name, "").strip().lower()
    if not raw:
        return default
    if raw in {"1", "true", "yes", "on"}:
        return True
    if raw in {"0", "false", "no", "off"}:
        return False
    logger.warning("invalid %s=%r; using default %s", name, raw, default)
    return default


@dataclass
class Settings:
    """Runtime configuration; mutable so tests can override a single field."""

    llm_timeout_seconds: float = 30.0
    llm_max_retries: int = 2
    tool_timeout_seconds: float = 15.0
    run_timeout_seconds: float = 180.0
    max_agents: int = 7
    require_approval: bool = True
    auto_approve: bool = False
    pending_ttl_hours: float = 24.0
    max_input_chars: int = 8000


def load_settings() -> Settings:
    """Read the environment once; invalid values fall back to defaults."""
    return Settings(
        llm_timeout_seconds=_float("LLM_TIMEOUT_SECONDS", 30.0, 0.001),
        llm_max_retries=_int("LLM_MAX_RETRIES", 2, 0),
        tool_timeout_seconds=_float("TOOL_TIMEOUT_SECONDS", 15.0, 0.001),
        run_timeout_seconds=_float("RUN_TIMEOUT_SECONDS", 180.0, 0.001),
        max_agents=_int("MAX_AGENTS", 7, 1),
        require_approval=_bool("REQUIRE_APPROVAL", True),
        auto_approve=_bool("AUTO_APPROVE", False),
        pending_ttl_hours=_float("PENDING_TTL_HOURS", 24.0, 0.0),
        max_input_chars=_int("MAX_INPUT_CHARS", 8000, 1),
    )


settings = load_settings()
