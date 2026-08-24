"""Differential harness: the ty-unnecessary fork vs basedpyright (v1 criteria 2+3).

Runs sightline's own ``provers.oracle.Oracle`` twice on a corpus repo — once with
basedpyright, once with the fork's pyright-CLI shim (``ty-unnecessary.exe``) — so both
sides get the identical config the production pipeline uses (which also discharges the
"consumable drop-in" criterion: no oracle change, just ``exe=``).

Diagnostics are compared on ``(rel, line, col, rule, polarity)``. Every non-match must be
classified in the divergence ledger (``parity/ty-divergences.toml``) as ``ty-better`` or
``ty-worse`` with grounding; an unclassified diff fails the run (v1 criterion 3: no silent
divergence).

Usage:
    python scripts/diff_pyright.py <repo-root> --fork <ty-unnecessary.exe> \
        [--ledger parity/ty-divergences.toml] [--receipt parity]

The receipt (match rate per rule + full divergence list) is written to
``<receipt>/<repo-name>.json`` for committing.

Cost: two full oracle passes over the repo, run sequentially (a basedpyright pass on the
largest corpus repo is ~2min; the fork pass is the thing being measured).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sys
import tempfile
import time
import tomllib
from pathlib import Path
from typing import NamedTuple

SIGHTLINE_ROOT = Path(
    os.environ.get(
        "SIGHTLINE_ROOT",
        Path(__file__).resolve().parent.parent.parent / "agent-codebase-checker",
    )
)
sys.path.insert(0, str(SIGHTLINE_ROOT / "src"))

from sightline.provers.oracle import Oracle, OracleDiag, detect_python_env  # noqa: E402

# v1 criterion 2: measured on the deps-resolved corpus 2026-08-23 (lol-predictor 81.2%
# over 16 pyright sites, ROFL-File-Information 100% over 55, merged-calculator 99.2%
# over 131; pooled 198/202 = 98.0%), pinned with margin. A run below its floor fails
# even with a fully classified ledger.
MATCH_RATE_FLOORS = {
    "lol-predictor": 0.80,
    "ROFL-File-Information": 0.95,
    "merged-calculator": 0.97,
}

POLARITY_PATTERNS = [
    (re.compile(r"always evaluate to True"), "true"),
    (re.compile(r"always evaluate to False"), "false"),
    (re.compile(r"is always an instance|is always a subclass"), "always"),
    (re.compile(r"is never an instance|is never a subclass"), "never"),
    (re.compile(r"references (?:function|coroutine) which always evaluates"), "truthy"),
]


class DiagKey(NamedTuple):
    rel: str
    line: int
    col: int
    rule: str
    polarity: str

    @classmethod
    def of(cls, diag: OracleDiag) -> DiagKey:
        polarity = ""
        for pattern, name in POLARITY_PATTERNS:
            if pattern.search(diag.message):
                polarity = name
                break
        return cls(diag.rel, diag.line, diag.col, diag.rule, polarity)

    def ledger_id(self, repo: str, side: str) -> str:
        return f"{repo}:{self.rel}:{self.line}:{self.col}:{self.rule}:{side}"


class Comparison(NamedTuple):
    pyright_map: dict[DiagKey, OracleDiag]
    fork_map: dict[DiagKey, OracleDiag]
    matched: list[DiagKey]
    only_pyright: list[DiagKey]
    only_fork: list[DiagKey]
    pyright_wall: float
    fork_wall: float


def corpus_excludes(root: Path) -> list[str]:
    config = SIGHTLINE_ROOT / "corpus" / f"{root.name}.toml"
    if not config.exists():
        return []
    data = tomllib.loads(config.read_text(encoding="utf-8"))
    return data.get("tool", {}).get("sightline", {}).get("excludes", [])


def basedpyright_exe() -> Path:
    """The comparator, resolved explicitly: sightline's `default_exe` is the
    fork shim since the production switch, so relying on it here would
    silently self-compare (100% "bit-identity" of the fork against itself)."""
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


def run_side(
    root: Path, excludes: list[str], python_exe: Path, exe: Path | None, label: str
) -> tuple[list[OracleDiag], float]:
    started = time.monotonic()
    oracle = Oracle(root, excludes=excludes, python_exe=python_exe, exe=exe)
    diags = oracle.unnecessary()
    wall = time.monotonic() - started
    print(f"  {label}: {len(diags)} reportUnnecessary* diagnostics in {wall:.1f}s")
    return diags, wall


def compare(root: Path, fork_exe: Path) -> Comparison:
    """Both oracle passes, sequentially (trap ledger: concurrent sidecars starve — keep
    everything else off the oracle while this runs; a starved sidecar wedges, it doesn't
    fail).

    Both sides analyze one shared *shadow tree* (sources only), exactly like the
    production fused pass: a real-tree run makes basedpyright enumerate `.venv`
    (multi-GB on ML repos) and never finish."""
    excludes = corpus_excludes(root)
    python_exe = detect_python_env(root, None)
    if python_exe is None:
        raise SystemExit(
            f"FAIL: {root.name} has no resolvable python env; bit-identity is only "
            "measured deps-resolved (plan: banned shortcuts)"
        )
    print(f"{root.name}: excludes={excludes} python={python_exe}")
    with tempfile.TemporaryDirectory(prefix="ty-parity-") as td:
        shadow = Path(td) / "tree"
        Oracle(root, excludes=excludes, python_exe=python_exe).make_shadow(shadow)
        pyright_diags, pyright_wall = run_side(
            shadow, excludes, python_exe, exe=basedpyright_exe(),
            label="basedpyright",
        )
        fork_diags, fork_wall = run_side(
            shadow, excludes, python_exe, exe=fork_exe, label="ty-unnecessary"
        )
    pyright_map = {DiagKey.of(d): d for d in pyright_diags}
    fork_map = {DiagKey.of(d): d for d in fork_diags}
    return Comparison(
        pyright_map=pyright_map,
        fork_map=fork_map,
        matched=sorted(set(pyright_map) & set(fork_map)),
        only_pyright=sorted(set(pyright_map) - set(fork_map)),
        only_fork=sorted(set(fork_map) - set(pyright_map)),
        pyright_wall=pyright_wall,
        fork_wall=fork_wall,
    )


def classify_divergences(
    comparison: Comparison, repo: str, ledger: dict[str, dict[str, str]]
) -> tuple[list[dict], list[str], dict[str, int]]:
    """Look every divergence up in the ledger; return (records, unclassified, counts)."""
    divergences: list[dict] = []
    unclassified: list[str] = []
    counts = {"ty-better": 0, "ty-worse": 0}
    for side, keys, source in (
        ("missing", comparison.only_pyright, comparison.pyright_map),
        ("extra", comparison.only_fork, comparison.fork_map),
    ):
        for key in keys:
            ident = key.ledger_id(repo, side)
            entry = ledger.get(ident)
            divergences.append(
                {
                    "id": ident,
                    "side": side,
                    "rule": key.rule,
                    "polarity": key.polarity,
                    "message": source[key].message,
                    "class": entry.get("class") if entry else None,
                }
            )
            if entry is None:
                unclassified.append(ident)
            else:
                counts[entry["class"]] += 1
    return divergences, unclassified, counts


def write_receipt(
    path: Path, repo: str, fork_exe: Path, comparison: Comparison, divergences: list[dict]
) -> Path:
    per_rule = {}
    rules = {k.rule for k in comparison.pyright_map} | {k.rule for k in comparison.fork_map}
    for rule in sorted(rules):
        per_rule[rule] = {
            "pyright": sum(1 for k in comparison.pyright_map if k.rule == rule),
            "fork": sum(1 for k in comparison.fork_map if k.rule == rule),
            "matched": sum(1 for k in comparison.matched if k.rule == rule),
        }
    total = len(comparison.pyright_map)
    receipt = {
        "repo": repo,
        "fork_exe": str(fork_exe),
        "pyright_total": total,
        "fork_total": len(comparison.fork_map),
        "matched": len(comparison.matched),
        "match_rate": round(len(comparison.matched) / total, 4) if total else 1.0,
        "per_rule": per_rule,
        "pyright_wall_s": round(comparison.pyright_wall, 1),
        "fork_wall_s": round(comparison.fork_wall, 1),
        "divergences": divergences,
    }
    path.mkdir(parents=True, exist_ok=True)
    receipt_path = path / f"{repo}.json"
    receipt_path.write_text(json.dumps(receipt, indent=2), encoding="utf-8")
    return receipt_path


def report(args: argparse.Namespace, comparison: Comparison) -> int:
    """Classify against the ledger, write the receipt, fail on unclassified diffs."""
    repo = args.root.resolve().name
    ledger = (
        tomllib.loads(args.ledger.read_text(encoding="utf-8")) if args.ledger.exists() else {}
    )
    divergences, unclassified, counts = classify_divergences(comparison, repo, ledger)

    total = len(comparison.pyright_map)
    rate = len(comparison.matched) / total if total else 1.0
    print(
        f"  matched {len(comparison.matched)}/{total} pyright sites ({rate:.1%}); "
        f"fork-extra {len(comparison.only_fork)}; ty-better {counts['ty-better']} / "
        f"ty-worse {counts['ty-worse']}; unclassified {len(unclassified)}"
    )
    receipt_path = write_receipt(args.receipt, repo, args.fork, comparison, divergences)
    print(f"  receipt: {receipt_path}")

    if unclassified:
        print(f"FAIL: {len(unclassified)} unclassified divergences (add to {args.ledger}):")
        for ident in unclassified[:40]:
            print(f"    {ident}")
        return 1
    floor = MATCH_RATE_FLOORS.get(repo)
    if floor is not None and rate < floor:
        print(f"FAIL: match rate {rate:.1%} fell below the pinned floor {floor:.0%}")
        return 1
    return 0


def main() -> int:
    """Run both oracle passes and report. Cost: two full checker passes, sequential."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--fork", type=Path, required=True)
    parser.add_argument("--ledger", type=Path, default=Path(__file__).resolve().parent.parent / "parity" / "ty-divergences.toml")
    parser.add_argument("--receipt", type=Path, default=Path(__file__).resolve().parent.parent / "parity")
    args = parser.parse_args()
    comparison = compare(args.root.resolve(), args.fork.resolve())
    return report(args, comparison)


if __name__ == "__main__":
    sys.exit(main())
