# Unnecessary comparisons

Port of pyright's `reportUnnecessaryComparison` (checker.ts `_validateComparisonTypes`,
`typeEvaluator.ts` `typesOverlap` / `isTypeComparable`): `==` / `!=` / `is` / `is not`
comparisons whose operand types have no overlap, plus conditional expressions that test a
function or coroutine object.

## `==` / `!=` with no overlap

```toml
[rules]
unnecessary-comparison = "error"
```

```py
def _(x: str):
    x == 1  # error: [unnecessary-comparison]
    x != 1  # error: [unnecessary-comparison]
    1 == x  # error: [unnecessary-comparison]

def _(x: int | str):
    x == 1

def _(x: int):
    x == 1
```

## `is` / `is not` with `None`

```toml
[rules]
unnecessary-comparison = "error"
```

```py
def _(x: str):
    x is None  # error: [unnecessary-comparison]
    x is not None  # error: [unnecessary-comparison]

def _(x: str | None):
    x is None
```

## `bool` vs `int` literal carve-out

pyright treats `bool` as comparable to the int literals `0` and `1` only.

```toml
[rules]
unnecessary-comparison = "error"
```

```py
def _(b: bool):
    b == 1
    b == 0
    b == 2  # error: [unnecessary-comparison]
```

## Numeric promotions

pyright's `assignType` bakes in the int → float → complex promotions, so mixed-rank numeric
comparisons always overlap (`2.0 == 2` is `True` at runtime). Same-class comparisons of
disjoint literals still use the literal arm.

```toml
[rules]
unnecessary-comparison = "error"
```

```py
def _(v: float, n: int, c: complex, b: bool):
    v == n
    v == int(v)
    c == n
    c == v
    b == 1.0
```

## Strict numerics under narrowing

pyright distinguishes a *narrowed* `float` (the strict class an `isinstance` check produces)
from an *annotated* `float` (the promoted `int | float` form): the strict form does not
overlap an int literal, the promoted form does. Pinned against basedpyright 1.39.10 with
out-of-corpus probes; in ty the strict form is what a narrowing projection yields.

```toml
[rules]
unnecessary-comparison = "error"
```

```py
def narrowed(x) -> None:
    if isinstance(x, float):
        x == 0  # error: [unnecessary-comparison]
        n: int = 1
        x == n  # error: [unnecessary-comparison]

def annotated(v: float) -> None:
    v == 0
```

## Narrowed `Unknown` stays comparable

pyright has no intersection types: narrowing an `Unknown` value with `!=` leaves it
`Unknown` there, which is comparable to anything. ty's `Unknown & ~Literal[...]` must
project the same way.

```toml
[rules]
unnecessary-comparison = "error"
unnecessary-contains = "error"
```

```py
def _(x, keep: list[int]):
    if x != "--":
        x == 3
        x in keep
```

## User classes and `__eq__`

An `==` comparison of unrelated user-class instances is reported only when the left operand's
class does not define a custom `__eq__`. A dataclass-synthesized `__eq__` does not count.

```toml
[rules]
unnecessary-comparison = "error"
```

```py
from dataclasses import dataclass

class A: ...
class B: ...

class WithEq:
    def __eq__(self, other: object) -> bool:
        return True

@dataclass
class DC:
    x: int

def _(a: A, b: B, w: WithEq, dc: DC):
    a == b  # error: [unnecessary-comparison]
    w == a
    dc == b  # error: [unnecessary-comparison]
```

## Literal operands

```toml
[rules]
unnecessary-comparison = "error"
```

```py
from typing import Literal

def _(x: Literal["a"]):
    x == "b"  # error: [unnecessary-comparison]
    x == "a"
```

## Static conditions are exempt

pyright suppresses the literal-vs-literal arm for statically-evaluable platform / version
conditions (`evaluateStaticBoolExpression`).

```toml
[environment]
python-platform = "linux"

[rules]
unnecessary-comparison = "error"
```

```py
import sys

if sys.platform == "win32":
    ...
```

## Suppressed within `assert`

```toml
[rules]
unnecessary-comparison = "error"
```

```py
def _(x: str):
    assert x != 1
```

## Functions and coroutines in conditionals

pyright reports a condition that tests a function or coroutine object (always truthy) under
the same rule (checker.ts `_reportUnnecessaryConditionExpression`). The check recurses through
`and` / `or` / `not` and applies to `if` / `while` / ternary / comprehension-if conditions.

```toml
[rules]
unnecessary-comparison = "error"
```

```py
import asyncio

def f() -> int:
    return 1

async def g() -> int:
    return 1

def _(x: int):
    if f:  # error: [unnecessary-comparison]
        ...
    if f():
        ...
    if not f:  # error: [unnecessary-comparison]
        ...
    if f and x:  # error: [unnecessary-comparison]
        ...
    y = 1 if f else 2  # error: [unnecessary-comparison]
    z = [n for n in range(3) if f]  # error: [unnecessary-comparison]
    # last: an always-truthy `while` never exits, so anything after it would be
    # unreachable (and diagnostics there are suppressed, matching pyright)
    while f:  # error: [unnecessary-comparison]
        ...

async def _():
    c = g()
    if c:  # error: [unnecessary-comparison]
        ...
    await c
```
