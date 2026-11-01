"""Safe arithmetic and date math used by planets through the Sun's tool loop.

Nothing is ``eval``'d: the expression is parsed with :mod:`ast` and every node
must be on the allowlist, so a model cannot reach names, attributes or the
import machinery. Only abs/round/min/max and two date helpers are callable.
"""

from __future__ import annotations

import ast
import math
from datetime import date
from typing import Any

MAX_EXPONENT = 12
DATE_FUNCS = {"days_between", "today"}
NUMERIC_FUNCS = {"abs", "round", "min", "max"}
ALLOWED_CALLS = DATE_FUNCS | NUMERIC_FUNCS

BIN_OPS = {
    ast.Add: lambda left, right: left + right,
    ast.Sub: lambda left, right: left - right,
    ast.Mult: lambda left, right: left * right,
    ast.Div: lambda left, right: left / right,
    ast.FloorDiv: lambda left, right: left // right,
    ast.Mod: lambda left, right: left % right,
    ast.Pow: lambda left, right: left**right,
}
UNARY_OPS = {ast.UAdd: lambda value: +value, ast.USub: lambda value: -value}


class CalcError(RuntimeError):
    """The expression is unsupported, malformed, or does not evaluate."""


def _days_between(start: str, end: str) -> int:
    return (date.fromisoformat(str(end)) - date.fromisoformat(str(start))).days


def _evaluate(node: ast.AST) -> Any:
    if isinstance(node, ast.Expression):
        return _evaluate(node.body)
    if isinstance(node, ast.Constant):
        if isinstance(node.value, bool) or not isinstance(node.value, (int, float, str)):
            raise CalcError("unsupported expression")
        return node.value
    if isinstance(node, ast.UnaryOp) and type(node.op) in UNARY_OPS:
        return UNARY_OPS[type(node.op)](_evaluate(node.operand))
    if isinstance(node, ast.BinOp) and type(node.op) in BIN_OPS:
        left = _evaluate(node.left)
        right = _evaluate(node.right)
        if isinstance(node.op, ast.Pow) and abs(right) > MAX_EXPONENT:
            raise CalcError(f"exponent above {MAX_EXPONENT}")
        return BIN_OPS[type(node.op)](left, right)
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
        name = node.func.id
        if node.keywords or name not in ALLOWED_CALLS:
            raise CalcError("unsupported expression")
        args = [_evaluate(arg) for arg in node.args]
        try:
            if name == "today":
                if args:
                    raise CalcError("today() takes no arguments")
                return date.today().isoformat()
            if name == "days_between":
                if len(args) != 2:
                    raise CalcError("days_between() takes two ISO dates")
                return _days_between(args[0], args[1])
            return {"abs": abs, "round": round, "min": min, "max": max}[name](*args)
        except (TypeError, ValueError, OverflowError) as exc:
            raise CalcError(str(exc)) from exc
    raise CalcError("unsupported expression")


def _format(value: Any) -> str:
    if isinstance(value, bool):
        raise CalcError("unsupported expression")
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        if math.isnan(value) or math.isinf(value):
            raise CalcError("result is not a finite number")
        return str(round(value, 6))
    return str(value)


def calculate(expression: str) -> dict[str, str]:
    """Evaluate one arithmetic/date expression and return ``{expression, value}``."""
    cleaned = (expression or "").strip()
    if not cleaned:
        raise CalcError("empty expression")
    if len(cleaned) > 500:
        raise CalcError("expression too long")
    try:
        tree = ast.parse(cleaned, mode="eval")
    except SyntaxError as exc:
        raise CalcError(f"invalid syntax: {exc.msg}") from exc
    return {"expression": cleaned, "value": _format(_evaluate(tree))}
