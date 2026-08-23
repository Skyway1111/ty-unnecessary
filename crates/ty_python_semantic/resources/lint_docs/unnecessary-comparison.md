## What it does

Detects comparisons that always evaluate to the same result because the operand types have no
overlap.

This is a port of pyright's `reportUnnecessaryComparison` check, covering:

- `==` / `!=` and `is` / `is not` comparisons whose operand types have no overlap
  (checker.ts `_validateComparisonTypes` / `isTypeComparable`), and
- conditional expressions that test a function or coroutine object, which is always truthy
  (checker.ts `_reportUnnecessaryConditionExpression`).

Like pyright, the `==` / `!=` arm deliberately ignores user-defined `__eq__` methods on
disjoint builtin types; consumers that need soundness must demote those results themselves.

Comparisons inside `assert` statements are not reported.

## Why is this bad?

A comparison with a statically-known result is dead code, and often indicates comparing values
of the wrong type (for example an enum member against a plain string).

## Example

```python
def f(x: str):
    if x == 1:  # error: [unnecessary-comparison]
        ...
```
