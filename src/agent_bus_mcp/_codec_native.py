from __future__ import annotations

import json
from typing import Any


def get_backend_name() -> str:
    return "python"


def dumps_pretty(data: Any) -> str:
    return json.dumps(data, indent=2)


def dumps_compact(data: Any) -> str:
    return json.dumps(data, separators=(",", ":"))

