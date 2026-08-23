## What it does

Detects `isinstance()` and `issubclass()` calls whose result is statically known to always be
`True` or always be `False`.

This is a port of pyright's `reportUnnecessaryIsInstance` check: the first argument is narrowed
by the classinfo argument in both polarities, and the call is reported when either narrowing
produces `Never`.

Calls inside `assert` statements are not reported.

## Why is this bad?

The check is dead code: it either always passes or can never pass, which usually indicates a
stale defensive check or a misunderstanding of the value's type.

## Example

```python
def f(x: int):
    if isinstance(x, int):  # error: [unnecessary-isinstance]
        ...
```
