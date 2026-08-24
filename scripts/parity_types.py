"""reveal-type parity: sightline's argtypes/rettypes queries against the fork (v1 criterion 5).

Runs sightline's full ``gate.collect`` pipeline twice on a corpus repo — once with
basedpyright, once with the fork shim (``default_exe`` monkeypatched) — and compares the
established-type answers for the *actual* queries argtypes/rettypes generate, plus the
resulting findings-by-rule counts.

Answers are compared both exactly and after sightline's own type-string algebra
(``typestrings.split_union`` + ``deliteral``), since ty's display differs cosmetically from
pyright's (quote style in literals, ``<subclass of ...>`` synthetics).

Usage:  python scripts/parity_types.py <repo-root> --fork <ty-unnecessary.exe>

Cost: two full sightline collect passes (each one oracle run), sequential.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from collections import Counter
from pathlib import Path

SIGHTLINE_ROOT = Path(
    os.environ.get(
        "SIGHTLINE_ROOT",
        Path(__file__).resolve().parent.parent.parent / "agent-codebase-checker",
    )
)
sys.path.insert(0, str(SIGHTLINE_ROOT / "src"))


def basedpyright_exe() -> Path:
    """The comparator, resolved explicitly: sightline's `default_exe` is the
    fork shim since the production switch — relying on it would self-compare."""
    import shutil

    local = Path(sys.executable).parent / (
        "basedpyright.exe" if sys.platform == "win32" else "basedpyright"
    )
    if local.exists():
        return local
    found = shutil.which("basedpyright")
    if not found:
        raise SystemExit(
            "FAIL: basedpyright not found — install sightline's [parity] extra"
        )
    return Path(found)


def run_collect(root: Path, exe_override: Path | None):
    """One full sightline collect; returns (answers, findings_by_rule, notes).
    exe_override None = the basedpyright comparator side."""
    import sightline.provers.oracle as oracle_module
    from sightline.config import load_config
    from sightline.gate import collect

    original = oracle_module.default_exe
    exe = exe_override if exe_override is not None else basedpyright_exe()
    oracle_module.default_exe = lambda: exe
    try:
        config = load_config(root)
        facts, provers, kept, _suppressed, metrics = collect(root, config)
    finally:
        oracle_module.default_exe = original
    answers = dict(getattr(provers.oracle, "_answers", {})) if provers.oracle else {}
    by_rule = Counter(f.rule for f in kept)
    return answers, by_rule, list(provers.notes)


def main() -> int:
    """Two collect passes + comparison. Cost: two oracle runs, sequential."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--fork", type=Path, required=True)
    parser.add_argument(
        "--receipt", type=Path,
        default=Path(__file__).resolve().parent.parent / "parity",
    )
    args = parser.parse_args()
    root = args.root.resolve()

    from sightline.provers.typestrings import deliteral, split_union

    def normalized(type_str: str) -> tuple:
        return tuple(sorted(base for part in split_union(type_str) for base in deliteral(part)))

    print(f"{root.name}: basedpyright pass")
    started = time.monotonic()
    base_answers, base_rules, base_notes = run_collect(root, None)
    base_wall = time.monotonic() - started
    print(f"{root.name}: fork pass ({base_wall:.1f}s for basedpyright)")
    started = time.monotonic()
    fork_answers, fork_rules, fork_notes = run_collect(root, args.fork.resolve())
    fork_wall = time.monotonic() - started
    print(f"  collect walls: basedpyright {base_wall:.1f}s, fork {fork_wall:.1f}s")

    shared = sorted(set(base_answers) & set(fork_answers))
    exact = sum(1 for q in shared if base_answers[q] == fork_answers[q])
    equivalent = sum(
        1 for q in shared if normalized(base_answers[q]) == normalized(fork_answers[q])
    )
    only_base = sorted(set(base_answers) - set(fork_answers))
    only_fork = sorted(set(fork_answers) - set(base_answers))
    diffs = [
        {"query": q, "pyright": base_answers[q], "fork": fork_answers[q]}
        for q in shared
        if normalized(base_answers[q]) != normalized(fork_answers[q])
    ]

    print(
        f"  answers: {len(base_answers)} pyright / {len(fork_answers)} fork; "
        f"shared {len(shared)}: exact {exact}, algebra-equivalent {equivalent}; "
        f"only-pyright {len(only_base)}, only-fork {len(only_fork)}"
    )
    print(f"  findings by rule (pyright): {dict(sorted(base_rules.items()))}")
    print(f"  findings by rule (fork):    {dict(sorted(fork_rules.items()))}")

    receipt = {
        "repo": root.name,
        "queries_pyright": len(base_answers),
        "queries_fork": len(fork_answers),
        "shared": len(shared),
        "exact": exact,
        "algebra_equivalent": equivalent,
        "only_pyright": only_base,
        "only_fork": only_fork,
        "answer_diffs": diffs,
        "findings_by_rule_pyright": dict(sorted(base_rules.items())),
        "findings_by_rule_fork": dict(sorted(fork_rules.items())),
        "notes_pyright": base_notes,
        "notes_fork": fork_notes,
        "collect_wall_s_pyright": round(base_wall, 1),
        "collect_wall_s_fork": round(fork_wall, 1),
    }
    args.receipt.mkdir(parents=True, exist_ok=True)
    path = args.receipt / f"{root.name}-types.json"
    path.write_text(json.dumps(receipt, indent=2), encoding="utf-8")
    print(f"  receipt: {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
