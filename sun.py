#!/usr/bin/env python3
"""Terminal entry point for the Sun orchestrator.

Usage:
    python sun.py "task"                 # hardcoded researcher -> writer -> checker
    python sun.py "task" --plan          # LLM-chosen plan from agents/sun.md
    python sun.py --file examples/inquiry.txt  # read the task from a file
    python sun.py                        # use TASK_DESCRIPTION from .env
    python sun.py --pending              # list actions awaiting approval
    python sun.py --approve <id>         # execute one stored action once
    python sun.py --reject <id>          # reject one stored action
    python sun.py --show <run_id>        # readable run timeline

Logger and notifier always run last, so every run is saved to data/sol-o.db.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
from pathlib import Path

from dotenv import load_dotenv

from app import approvals
from app import logger as run_store
from app.orchestrator import orchestrate
from app.reliability import install_redaction_filter

ROOT = Path(__file__).resolve().parent
EXIT_CODES = {
    "complete": 0,
    "failed": 1,
    "needs_clarification": 2,
    "partial": 3,
    "awaiting_approval": 4,
    "rejected": 5,
}


def _task_from_args(parser: argparse.ArgumentParser, args: argparse.Namespace) -> str:
    if args.file is not None:
        try:
            task = args.file.read_text(encoding="utf-8").strip()
        except OSError as exc:
            parser.error(f"cannot read {args.file}: {exc}")
    elif args.task:
        task = " ".join(args.task).strip()
    else:
        task = os.getenv("TASK_DESCRIPTION", "").strip()
    if not task:
        parser.error("no task: pass text, use --file, or set TASK_DESCRIPTION in .env")
    return task


def _show_run(run_id: str) -> int:
    detail = run_store.get_run_detail(run_id)
    if detail is None:
        print(f"run {run_id} not found")
        return 1
    run = detail["run"]
    print(
        f"Run {run['id']}  status={run['status']}  created={run['created_at']}  "
        f"duration={run['duration_seconds']}s"
    )
    print(f"task: {run['task'][:300]}")
    if run.get("warning"):
        print(f"warning: {run['warning']}")
    print("timeline:")
    if not detail["agent_calls"]:
        print("  (no agent calls recorded)")
    for call in detail["agent_calls"]:
        line = (
            f"  step {call['step']:>2}  {call['agent']:<10} {call['status']:<7} "
            f"{call['duration_ms']:>7}ms  attempts={call['attempts']}"
        )
        if call["error_type"]:
            line += f"  error={call['error_type']}: {call['error_message']}"
        print(line)
    for tool in detail["tool_calls"]:
        line = (
            f"  tool      {tool['tool']:<10} {tool['status']:<7} "
            f"{tool['duration_ms']:>7}ms"
        )
        if tool["error_message"]:
            line += f"  error={tool['error_message']}"
        print(line)
    for action in detail["pending_actions"]:
        print(
            f"  approval  #{action['id']} {action['action_type']} "
            f"status={action['status']} decided={action['decided_at'] or '-'}"
        )
    return 0


def _show_pending() -> int:
    rows = approvals.list_pending()
    if not rows:
        print("no pending actions")
        return 0
    for row in rows:
        payload = approvals.payload_for_display(row)
        print(
            f"#{row['id']}  run={row['run_id']}  type={row['action_type']}  "
            f"created={row['created_at']}  run_status={payload['run_status']}"
        )
        print(f"  task: {payload['task'][:200]}")
        print(f"  verdict: {payload['verdict'] or 'n/a'}")
        print("  message to approve:")
        for line in str(payload["message"]).splitlines():
            print(f"    {line}")
    return 0


def _decide(action_id: int, decision: str) -> int:
    try:
        result = approvals.approve(action_id) if decision == "approve" else approvals.reject(
            action_id
        )
    except approvals.ApprovalError as exc:
        print(f"{decision} failed ({exc.code}): {exc}")
        return 1
    print(json.dumps(result, indent=2, default=str))
    return 0


def main(argv: list[str] | None = None) -> int:
    load_dotenv(ROOT / ".env")
    parser = argparse.ArgumentParser(
        prog="sun.py",
        description="Run a task through the Sun orchestrator or manage approvals.",
    )
    parser.add_argument("task", nargs="*", help="the task to run")
    parser.add_argument(
        "--file",
        type=Path,
        help="read the task from a text file instead of the command line",
    )
    parser.add_argument(
        "--plan",
        action="store_true",
        dest="use_llm_planner",
        help="let agents/sun.md choose the agents instead of the hardcoded flow",
    )
    actions = parser.add_mutually_exclusive_group()
    actions.add_argument("--show", metavar="RUN_ID", help="print a run timeline")
    actions.add_argument("--pending", action="store_true", help="list pending actions")
    actions.add_argument("--approve", type=int, metavar="ID", help="approve one action")
    actions.add_argument("--reject", type=int, metavar="ID", help="reject one action")
    args = parser.parse_args(argv)

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )
    install_redaction_filter()

    if args.show:
        return _show_run(args.show)
    if args.pending:
        return _show_pending()
    if args.approve is not None:
        return _decide(args.approve, "approve")
    if args.reject is not None:
        return _decide(args.reject, "reject")

    task = _task_from_args(parser, args)
    result = orchestrate(task, use_llm_planner=args.use_llm_planner)
    print(json.dumps(result.model_dump(), indent=2, default=str))
    return EXIT_CODES[result.status]


if __name__ == "__main__":
    raise SystemExit(main())
