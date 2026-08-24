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

## Annotated and async non-generator functions are untouched

```py
def annotated(x: int) -> int:
    return x

async def coro(x: int):
    return x

reveal_type(annotated)  # revealed: def annotated(x: int) -> int
reveal_type(coro)  # revealed: def coro(x: int) -> CoroutineType[Any, Any, Unknown]
```

## Generators reveal `Generator[yield, send, return]`

pyright infers `Generator[Y, S, R]` for unannotated generators: Y the union of yield
types, R the same returns-plus-fallthrough union as plain functions. Pinned against
basedpyright 1.39.10 out-of-corpus probes (2026-08-23).

```py
def plain(n: int):
    for i in range(n):
        yield i

def two_types(flag: bool):
    if flag:
        yield 1
    else:
        yield "s"

def with_return(n: int):
    yield n
    return "done"

def yields_none():
    yield

def untyped(x):
    yield x

# Known display divergences, both semantically faithful: pyright spells the
# narrowed falsy int `Literal[0]` where ty keeps the intersection form, and
# pyright drops statically-unreachable yields to `Never` where ty types them.
def narrowed(n: int):
    if n:
        return
    yield n

def unreachable_yield():
    if False:
        yield 1

reveal_type(narrowed)  # revealed: (n: int) -> Generator[int & ~AlwaysTruthy, Any, None]
reveal_type(unreachable_yield)  # revealed: () -> Generator[Literal[1], Any, None]
reveal_type(plain)  # revealed: (n: int) -> Generator[int, Any, None]
reveal_type(two_types)  # revealed: (flag: bool) -> Generator[Literal[1, "s"], Any, None]
reveal_type(with_return)  # revealed: (n: int) -> Generator[int, Any, Literal["done"]]
reveal_type(yields_none)  # revealed: () -> Generator[None, Any, None]
reveal_type(untyped)  # revealed: (x) -> Generator[Unknown, Any, None]
```

## Send slot: `Any` for statement yields, `Unknown` once a yield value is consumed

pyright shows `Any` in the send slot only while every yield is directly an
expression-statement value (parentheses included); a yield whose value is assigned,
tested, or otherwise consumed — and any `yield from` — flips it to `Unknown`.

```py
def consumed(n: int):
    got = yield n
    yield got

def tested(n: int):
    if (yield n):
        pass

def parenthesized(n: int):
    (yield n)

reveal_type(consumed)  # revealed: (n: int) -> Generator[int | Unknown, Unknown, None]
reveal_type(tested)  # revealed: (n: int) -> Generator[int, Unknown, None]
reveal_type(parenthesized)  # revealed: (n: int) -> Generator[int, Any, None]
```

## `yield from` contributes the delegate's element type

```py
from typing import Iterable

def delegates(xs: Iterable[str]):
    yield from xs

def mixed(xs: Iterable[str]):
    yield 1
    yield from xs

def with_result(xs: Iterable[str]):
    yield from xs
    return 3

reveal_type(delegates)  # revealed: (xs: Iterable[str]) -> Generator[str, Unknown, None]
reveal_type(mixed)  # revealed: (xs: Iterable[str]) -> Generator[Literal[1] | str, Unknown, None]
reveal_type(with_result)  # revealed: (xs: Iterable[str]) -> Generator[str, Unknown, Literal[3]]
```

## Async generators reveal `AsyncGenerator[yield, send]`

```py
async def agen(n: int):
    yield n

async def agen_used(n: int):
    got = yield n

reveal_type(agen)  # revealed: (n: int) -> AsyncGenerator[int, Any]
reveal_type(agen_used)  # revealed: (n: int) -> AsyncGenerator[int, Unknown]
```

## Nested defs do not leak yields; yields inside them do not count

A generator's Y/S come only from its own scope: a nested generator's yields belong to
the nested function.

```py
def outer(n: int):
    def inner():
        got = yield "s"
    yield n

reveal_type(outer)  # revealed: (n: int) -> Generator[int, Any, None]
```
