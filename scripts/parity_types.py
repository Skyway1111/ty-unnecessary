"""reveal-type parity: sightline's argtypes/rettypes queries against the fork (v1 criterion 5).

Runs sightline's full ``gate.collect`` pipeline twice on a corpus repo — once with
basedpyright, once with the fork shim (``default_exe`` monkeypatched) — and compares the
established-type answers for the *actual* queries argtypes/rettypes generate, plus the
resulting findings-by-rule counts.

Answers are compared both exactly and after sightline's own type-string algebra
(``typestrings.split_union`` + ``deliteral``), since ty's display differs cosmetically from
pyright's (quote style in literals, ``<subclass of ...>`` synthetics).

Usage:  python scripts/parity_types.py <repo-root> --fork <ty-unnecessary.exe>

Cost: two full sightline collect passes, sequential. The basedpyright side
runs the harness-local legacy transport, whose counterfactual worlds are full
shadow re-checks — one basedpyright pass per world (merged + suspects), ~12
minutes on ROFL-File-Information. Build-time only; budget accordingly.
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


def make_legacy_oracle_class():
    """The reveal-injection transport, harness-local: sightline's oracle now
    speaks the fork's batch protocol, which basedpyright cannot. This subclass
    restores the shadow-tree transport (wrap/EOF-append reveals, column
    back-mapping, worlds as full shadow re-checks) so the comparator side of
    this harness keeps running. It lives here because this harness is its only
    consumer (batch-protocol plan ruling)."""
    import re
    import tempfile

    from shadow import make_shadow
    from sightline.provers.oracle import Oracle, OracleDiag

    _REVEAL_RE = re.compile(r'Type of "(?s:.*)" is "(.*)"$')
    _PREFIX = len("reveal_type(")

    def _original_col(spans, col):
        acc = 0
        for s, e in spans:
            if col < s + acc:
                break
            if col < s + acc + _PREFIX:
                return s
            if col < e + acc + _PREFIX:
                return col - acc - _PREFIX
            acc += _PREFIX + 1
        return col - acc

    class LegacyOracle(Oracle):
        def _inject(self, shadow, queries):
            by_file = {}
            for q in queries:
                by_file.setdefault(q.rel, []).append(q)
            key_by_pos, spans = {}, {}
            for rel, file_queries in by_file.items():
                path = shadow / rel
                if not path.exists():
                    continue
                lines = path.read_text(encoding="utf-8").splitlines()
                wraps = [q for q in file_queries if q.expr is None]
                for q in sorted(wraps, key=lambda x: (x.line, -x.col_start)):
                    if q.line - 1 >= len(lines):
                        continue
                    ln = lines[q.line - 1]
                    if q.col_end > len(ln):
                        continue
                    lines[q.line - 1] = (
                        ln[: q.col_start] + "reveal_type("
                        + ln[q.col_start : q.col_end] + ")" + ln[q.col_end :]
                    )
                    key_by_pos[(rel, q.line)] = q.id
                    spans.setdefault((rel, q.line), []).append(
                        (q.col_start, q.col_end)
                    )
                for q in file_queries:
                    if q.expr is not None:
                        lines.append(f"reveal_type({q.expr})")
                        key_by_pos[(rel, len(lines))] = q.id
                path.write_text("\n".join(lines) + "\n", encoding="utf-8")
            for line_spans in spans.values():
                line_spans.sort()
            return key_by_pos, spans

        def _ensure_pass(self, queries=None):
            if self._diags is not None:
                return
            if queries is None:
                queries = list(self.query_supplier()) if self.query_supplier else []
            if not queries:
                self._diags = self._run(self.root)
                return
            with tempfile.TemporaryDirectory(prefix="sightline-shadow-") as td:
                shadow = Path(td) / "tree"
                make_shadow(self.root, self.excludes, shadow)
                key_by_pos, spans = self._inject(shadow, queries)
                raw = self._run(shadow, label="diagnostics+types")
            self._attempted_ids = {q.id for q in queries}
            diags = []
            for d in raw:
                m = _REVEAL_RE.match(d.message)
                if m:
                    qid = key_by_pos.get((d.rel, d.line))
                    if qid is not None:
                        self._answers[qid] = m.group(1)
                    continue
                line_spans = spans.get((d.rel, d.line))
                if line_spans:
                    d = OracleDiag(
                        rel=d.rel, line=d.line,
                        col=_original_col(line_spans, d.col),
                        rule=d.rule, message=d.message, severity=d.severity,
                    )
                diags.append(d)
            self._diags = diags

        def established_types(self, queries):
            if not queries:
                return {}
            if self._diags is None:
                self._ensure_pass(queries)
            if {q.id for q in queries} <= self._attempted_ids:
                return {
                    q.id: self._answers[q.id]
                    for q in queries
                    if q.id in self._answers
                }
            with tempfile.TemporaryDirectory(prefix="sightline-shadow-") as td:
                shadow = Path(td) / "tree"
                make_shadow(self.root, self.excludes, shadow)
                key_by_pos, _spans = self._inject(shadow, queries)
                out = {}
                for d in self._run(shadow, label="types"):
                    m = _REVEAL_RE.match(d.message)
                    if m:
                        qid = key_by_pos.get((d.rel, d.line))
                        if qid is not None:
                            out[qid] = m.group(1)
                return out

        def verify_worlds(self, worlds):
            base = {(d.rel, d.line, d.rule) for d in self.diagnostics()}
            out = {}
            for wid, files in worlds:
                with tempfile.TemporaryDirectory(prefix="sightline-cf-") as td:
                    shadow = Path(td) / "tree"
                    make_shadow(self.root, self.excludes, shadow)
                    for rel, content in files.items():
                        target = shadow / rel
                        if target.exists():
                            target.write_text(content, encoding="utf-8")
                    diags = self._run(shadow, label=f"world {wid}")
                out[wid] = [
                    d for d in diags
                    if (d.rel, d.line, d.rule) not in base
                    and not _REVEAL_RE.match(d.message)
                ]
            return out

    return LegacyOracle


def run_collect(root: Path, exe_override: Path | None):
    """One full sightline collect; returns (answers, findings_by_rule, notes).
    exe_override None = the basedpyright comparator side (legacy transport)."""
    import sightline.provers.oracle as oracle_module
    from sightline.config import load_config
    from sightline.gate import collect

    original = oracle_module.default_exe
    original_cls = oracle_module.Oracle
    if exe_override is not None:
        exe = exe_override
    else:
        exe = basedpyright_exe()
        oracle_module.Oracle = make_legacy_oracle_class()
    oracle_module.default_exe = lambda: exe
    try:
        config = load_config(root)
        facts, provers, kept, _suppressed, metrics = collect(root, config)
    finally:
        oracle_module.default_exe = original
        oracle_module.Oracle = original_cls
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
