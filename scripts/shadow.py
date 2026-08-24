"""Type-surface shadow copy (.py/.pyi/py.typed, structure preserved) for the
parity harnesses. Prunes excluded directories during the walk (a .venv must
never be enumerated, let alone copied). Excludes match by name or by
root-relative path prefix (e.g. "_research/toolchain"). Lives here because
these harnesses are its only consumers: sightline's production oracle runs
the batch protocol on the real tree."""

from __future__ import annotations

import shutil
from pathlib import Path

_SKIP_DIRS = {"__pycache__", "venv", "node_modules", "site-packages"}


def make_shadow(root: Path, excludes: list[str], dest: Path) -> None:
    root = Path(root).resolve()
    extra = {e.strip("/") for e in excludes}

    def excluded(path: Path, name: str) -> bool:
        if name in extra:
            return True
        rel = path.relative_to(root).as_posix()
        return any(rel == e or rel.startswith(e + "/") for e in extra)

    def walk(d: Path) -> None:
        for path in sorted(d.iterdir()):
            name = path.name
            if path.is_dir():
                if name.startswith(".") or name in _SKIP_DIRS:
                    continue
                if excluded(path, name):
                    continue
                walk(path)
            elif path.suffix in (".py", ".pyi") or name == "py.typed":
                target = dest / path.relative_to(root)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, target)

    walk(root)
