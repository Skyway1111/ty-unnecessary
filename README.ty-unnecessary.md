# ty-unnecessary

A fork of [astral-sh/ruff](https://github.com/astral-sh/ruff) (MIT) that adds pyright's
`reportUnnecessary*` diagnostic family to ty, plus a pyright-CLI-compatible shim binary, so
[sightline](../agent-codebase-checker)'s oracle can run on a Rust checker instead of
basedpyright's Node sidecar.

## Provenance

- **Upstream**: `astral-sh/ruff`, MIT license (`LICENSE`). The buildable ty crates
  (`ty_python_semantic` et al.) live in the ruff repository; `astral-sh/ty` is a
  distribution shell.
- **Forked commit**: `11c76bf48fdac06b2f240cba502eda96da4dce77` (tag `0.16.4`).
- **Tracking policy**: our delta is a small commit stack on branch `unnecessary`, rebased
  onto tagged ruff releases — never merged — so the ported-rules diff stays legible.

## The delta

| Piece | Where | What it mirrors |
| --- | --- | --- |
| `unnecessary-isinstance` / `unnecessary-comparison` / `unnecessary-contains` lints | `crates/ty_python_semantic/src/types/unnecessary.rs` | pyright 1.1.412 (basedpyright 1.39.10) `packages/pyright-internal/src/analyzer/checker.ts`: `_validateIsInstanceCall`, `_validateComparisonTypes`, `_validateContainmentTypes`, `_reportUnnecessaryConditionExpression`; `typeEvaluator.ts`: `typesOverlap` / `isTypeComparable`; `typeGuards.ts`: `getElementTypeForContainerNarrowing` / `narrowTypeForContainerElementType`. pyright is licensed MIT (© Microsoft); the ported files cite the mirrored functions. |
| pyright-CLI shim binary `ty-unnecessary` | `crates/ty_pyright_shim` | basedpyright's `--outputjson --threads N --project <pyrightconfig.json> <root>` CLI and `generalDiagnostics` JSON, including pyright-format `reveal_type` messages. |
| Batch protocol (`--batch <request.json>`) | `crates/ty_pyright_shim/src/batch.rs` | sightline's native protocol: span queries `(file, line, byte-cols) -> type` resolved from real inference; expr queries via in-memory `reveal_type` appends; counterfactual "worlds" (full-file overlays checked incrementally on one salsa db) reporting diagnostics added vs the base check by `(file, line, rule)`. Contract pinned by `tests/batch.rs`; the pyright CLI mode stays for the parity harness. |
| Differential harness | `scripts/diff_pyright.py`, ledger + receipts in `parity/` | Runs sightline's `provers/oracle.py` against both checkers on the corpus; every divergence must be classified `ty-better` / `ty-worse` in `parity/ty-divergences.toml` or the run fails. |

The rules are **off by default** (`Level::Ignore`); the shim enables them. The `==`/`!=`
no-overlap arm deliberately replicates pyright's `__eq__`-ignoring unsoundness — bit-identity
with pyright is the v1 acceptance gate; sightline's grounding layer stays responsible for
demoting equality diagnostics.

Type names *inside* messages come from ty's display (`Literal["a"]` vs pyright's
`Literal['a']`); parity is measured on `(file, line, col, rule, polarity)`, not message text.

## Build

```
cargo build --release -p ty_pyright_shim   # target/release/ty-unnecessary.exe
cargo test -p ty_python_semantic --test mdtest -- unnecessary
```

The `-- unnecessary` filter is the fork's gate. The *full* mdtest suite carries 9
upstream failures expected under the reveal patch: tests asserting stock
`def f(...) -> Unknown` reveals for unannotated functions now see the inferred-return
display (`annotations/self`, `call/methods`, `class/super`, `cycle`,
`exception/control_flow`, `import/star`, `overloads`, `shadowing/function`,
`ty_extensions`).
