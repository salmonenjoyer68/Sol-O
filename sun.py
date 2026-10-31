#!/usr/bin/env python3
"""Terminal entry point for the Sun orchestrator.

Usage:
    python sun.py "task"          # hardcoded researcher -> writer -> checker
    python sun.py "task" --plan   # LLM-chosen plan from agents/sun.md
"""

from __future__ import annotations

import argparse
import json
import logging

from app.orchestrator import orchestrate

EXIT_CODES = {"complete": 0, "failed": 1, "needs_clarification": 2}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="sun.py",
        description="Run a task through the Sun orchestrator.",
    )
    parser.add_argument("task", nargs="+", help="the task to run")
    parser.add_argument(
        "--plan",
        action="store_true",
        dest="use_llm_planner",
        help="let agents/sun.md choose the agents instead of the hardcoded flow",
    )
    args = parser.parse_args(argv)
    task = " ".join(args.task)

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )

    result = orchestrate(task, use_llm_planner=args.use_llm_planner)
    print(json.dumps(result.model_dump(), indent=2))
    return EXIT_CODES[result.status]


if __name__ == "__main__":
    raise SystemExit(main())
