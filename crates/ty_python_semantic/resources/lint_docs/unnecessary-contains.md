## What it does

Detects `in` / `not in` operations whose result is statically known because the tested element
type can never be present in the container.

This is a port of pyright's `reportUnnecessaryContains` check (checker.ts
`_validateContainmentTypes`): for specialized builtin containers (`list`, `set`, `frozenset`,
`deque`, `tuple`, `dict`, `defaultdict`, `OrderedDict`), the element operand is narrowed by the
container's element type; if the narrowing is `Never`, the operation is reported.

Containment tests inside `assert` statements are not reported.

## Why is this bad?

The membership test always evaluates to the same result, which usually means the wrong value or
the wrong container is being tested.

## Example

```python
def f(x: str, items: list[int]):
    if x in items:  # error: [unnecessary-contains]
        ...
```
