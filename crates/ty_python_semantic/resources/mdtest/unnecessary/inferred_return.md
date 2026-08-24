# Inferred returns in `reveal_type`

Fork delta for the sightline oracle: revealing an unannotated plain function shows the
body-inferred return (the union of return-expression types, plus `None` when the end of the
scope is reachable) instead of ty's stock `Unknown`, mirroring pyright's revealed
signatures. Sightline's #36/#40 oracle arms consume the inferred `Any`-vs-concrete
distinction.

## Unannotated functions reveal their inferred return

```py
from json import loads

def launders(path):
    return loads(path)

def predicate(q: str):
    return q.count("x")

def procedure(x: int):
    x + 1

def never_returns():
    raise ValueError

def maybe(flag: bool):
    if flag:
        return 1

reveal_type(launders)  # revealed: (path) -> Any
reveal_type(predicate)  # revealed: (q: str) -> int
reveal_type(procedure)  # revealed: (x: int) -> None
reveal_type(never_returns)  # revealed: () -> Never
reveal_type(maybe)  # revealed: (flag: bool) -> Literal[1] | None
```

## Untyped params keep an `Unknown` body inference

```py
def double(x):
    return x * 2

reveal_type(double)  # revealed: (x) -> Unknown
```

## Annotated, async, and generator functions are untouched

```py
def annotated(x: int) -> int:
    return x

async def coro(x: int):
    return x

def gen(x: int):
    yield x

reveal_type(annotated)  # revealed: def annotated(x: int) -> int
reveal_type(coro)  # revealed: def coro(x: int) -> CoroutineType[Any, Any, Unknown]
reveal_type(gen)  # revealed: def gen(x: int) -> Unknown
```
