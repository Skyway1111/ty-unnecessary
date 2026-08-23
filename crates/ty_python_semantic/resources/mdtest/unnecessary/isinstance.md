# Unnecessary `isinstance` / `issubclass` calls

Port of pyright's `reportUnnecessaryIsInstance` (checker.ts `_validateIsInstanceCall`): an
`isinstance()` / `issubclass()` call whose result is statically known is reported. The check
narrows the first argument by the classinfo argument in both polarities: if the negative
narrowing is `Never` the call is always true; if the positive narrowing is `Never` it is never
true.

## Always true

```toml
[rules]
unnecessary-isinstance = "error"
```

```py
def _(x: int):
    isinstance(x, int)  # error: [unnecessary-isinstance]

def _(x: bool):
    # bool is a subclass of int
    isinstance(x, int)  # error: [unnecessary-isinstance]

def _(x: int | str):
    isinstance(x, (int, str))  # error: [unnecessary-isinstance]
```

## Never true

```toml
[rules]
unnecessary-isinstance = "error"
```

`str` and `int` have conflicting instance layouts, so no runtime class can inherit from both:
the positive narrowing is `Never`.

```py
def _(x: str):
    isinstance(x, int)  # error: [unnecessary-isinstance]
```

## Ambiguous calls are not reported

```toml
[rules]
unnecessary-isinstance = "error"
```

```py
def _(x: object):
    isinstance(x, int)

def _(x: int | str):
    isinstance(x, int)

class A: ...
class B: ...

def _(x: A):
    # A common subclass of A and B is possible at runtime
    isinstance(x, B)
```

## `float` promotion

pyright expands promotion types on the first argument (`float` behaves as `int | float`), so
neither direction of a float/int check is reported; `bool` really is always an `int`.
Verified bit-identical with basedpyright 1.39.10.

```toml
[rules]
unnecessary-isinstance = "error"
```

```py
def _(x: float):
    isinstance(x, float)
    isinstance(x, int)

def _(x: bool):
    isinstance(x, int)  # error: [unnecessary-isinstance]
```

## `issubclass`

```toml
[rules]
unnecessary-isinstance = "error"
```

```py
def _(t: type[int]):
    issubclass(t, int)  # error: [unnecessary-isinstance]

def _(t: type):
    issubclass(t, int)
```

## Suppressed within `assert`

pyright skips the check when the call appears anywhere within an `assert` statement.

```toml
[rules]
unnecessary-isinstance = "error"
```

```py
def _(x: int):
    assert isinstance(x, int)
    assert x and isinstance(x, int)
```

## Only bare two-argument calls are checked

pyright only checks calls whose callee is literally the name `isinstance` / `issubclass`, with
exactly two arguments.

```toml
[rules]
unnecessary-isinstance = "error"
```

```py
import builtins

def _(x: int):
    builtins.isinstance(x, int)
    alias = isinstance
    alias(x, int)
```
