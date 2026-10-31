#!/usr/bin/env python3
"""Terminal client: POST a task to /run and validate the returned JSON."""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

from pydantic import ValidationError

from app.schemas import AgentOutput

DEFAULT_URL = "http://127.0.0.1:8000/run"


def main() -> int:
    if len(sys.argv) < 2:
        print('usage: python -m tests.test_run "<task>"', file=sys.stderr)
        return 2
    task = " ".join(sys.argv[1:])
    url = os.getenv("SOL_O_URL", DEFAULT_URL)
    payload = json.dumps({"agent": "researcher", "task": task}).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            body = response.read().decode("utf-8")
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        print(body)
        print(f"HTTP {exc.code}", file=sys.stderr)
        return 1
    except urllib.error.URLError as exc:
        print(f"request failed: {exc.reason}", file=sys.stderr)
        return 1

    try:
        output = AgentOutput.model_validate(json.loads(body))
    except (json.JSONDecodeError, ValidationError) as exc:
        print(body)
        print(f"invalid response: {exc}", file=sys.stderr)
        return 1

    print(json.dumps(output.model_dump(), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
