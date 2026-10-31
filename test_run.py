#!/usr/bin/env python3
"""Compatibility entry point for ``python test_run.py "<task>"``.

The terminal client lives in ``tests.test_run``; this wrapper keeps the
documented root-level command working without duplicating it.
"""

from tests.test_run import main

if __name__ == "__main__":
    raise SystemExit(main())
