# Unnecessary containment tests

Port of pyright's `reportUnnecessaryContains` (checker.ts `_validateContainmentTypes`,
`typeGuards.ts` `getElementTypeForContainerNarrowing` / `narrowTypeForContainerElementType`):
an `in` / `not in` test against a specialized builtin container whose element type can never
match the tested value.

## No overlap with the element type

```toml
[rules]
unnecessary-contains = "error"
```

```py
def _(x: str, items: list[int]):
    x in items  # error: [unnecessary-contains]
    x not in items  # error: [unnecessary-contains]

def _(x: int, items: list[int]):
    x in items

def _(x: int | str, items: list[int]):
    x in items
```

## Supported containers only

Only specialized builtin containers participate; custom containers and unspecialized ones are
never reported.

```toml
[rules]
unnecessary-contains = "error"
```

```py
from collections import deque

class Container:
    def __contains__(self, item: object) -> bool:
        return False

def _(x: str, s: set[int], d: dict[int, str], q: deque[int], t: tuple[int, float], c: Container):
    x in s  # error: [unnecessary-contains]
    x in d  # error: [unnecessary-contains]
    x in q  # error: [unnecessary-contains]
    x in t  # error: [unnecessary-contains]
    x in c
```

## Narrowed enums stay comparable

ty narrows `x is not E.NOPE` to an enum complement; pyright sees the remaining literal
union, which overlaps a container of the same enum.

```toml
[rules]
unnecessary-contains = "error"
```

```py
import enum

class E(enum.Enum):
    A = 1
    B = 2
    NOPE = 3

LIFE = frozenset({E.A, E.B})

def f(x: E, claimed: set[E]) -> None:
    if x is not E.NOPE:
        if x in claimed:
            pass
        if x in LIFE:
            pass
```

## Suppressed within `assert`

```toml
[rules]
unnecessary-contains = "error"
```

```py
def _(x: str, items: list[int]):
    assert x not in items
```
