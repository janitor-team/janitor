from collections.abc import Awaitable
from typing import Any

def run_diffoscope(
    old_binaries: list[tuple[str, str]],
    new_binaries: list[tuple[str, str]],
    timeout: float | None = None,
    memory_limit: int | None = None,
    diffoscope_command: str | None = None,
) -> Awaitable[dict[str, Any]]: ...
def filter_boring_udiff(
    udiff: str, old_version: str, new_version: str, display_version: str
) -> str: ...
