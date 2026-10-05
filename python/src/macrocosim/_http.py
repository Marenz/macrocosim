"""Thin synchronous client over macrocosim's HTTP ``/api/*`` control plane.

Internal: users reach these calls through :class:`macrocosim.runtime.Site`.
Each method maps to one endpoint and returns parsed JSON.
"""

from __future__ import annotations

from typing import Any, TypedDict

import httpx

from .errors import ControlRejected


class EvalResult(TypedDict, total=False):
    """An eval's outcome. ``ok`` is False (with ``error`` set) when
    the interpreter rejected the form; ``value`` is the printed
    result."""

    ok: bool
    value: Any
    error: str


def _error_text(resp: httpx.Response) -> str:
    """The ``error`` field of a failed response, or its text."""
    try:
        return str(resp.json().get("error", resp.text))
    except ValueError:
        return resp.text


def control_path(component_id: int, action: str, mg_id: int) -> str:
    """Route for one component action on microgrid ``mg_id``.

    One place builds the route for both client flavors, so a route change
    on the server cannot be missed in one of them.
    """
    return f"/api/mg/{mg_id}/component/{component_id}/{action}"


class HttpClient:
    """Blocking ``httpx`` client bound to a macrocosim UI server."""

    def __init__(self, base_url: str, *, timeout: float = 10.0) -> None:
        self._client = httpx.Client(base_url=base_url, timeout=timeout)

    def get_json(self, path: str) -> Any:
        """GET ``path`` and return the parsed JSON (shape is endpoint-specific)."""
        resp = self._client.get(path)
        resp.raise_for_status()
        return resp.json()

    def post(self, path: str, content: str = "") -> Any:
        resp = self._client.post(path, content=content)
        resp.raise_for_status()
        return resp.json() if resp.content else {}

    def control(self, path: str, payload: dict[str, Any]) -> Any:
        """POST a typed control request; a 4xx rejection raises.

        The control endpoints report rejections as structured JSON
        (``{"error": ...}``) with a 400/404 status — turn that into
        :class:`ControlRejected` so a rejection can never silently no-op.
        """
        resp = self._client.post(path, json=payload)
        if 400 <= resp.status_code < 500:
            raise ControlRejected(_error_text(resp))
        resp.raise_for_status()
        return resp.json() if resp.content else {}

    def eval(self, expr: str, mg_id: int | None = None) -> EvalResult:
        """POST a Lisp form: the whole-site ``/api/eval`` without an
        ``mg_id``, the microgrid's eval with one."""
        path = "/api/eval" if mg_id is None else f"/api/mg/{mg_id}/eval"
        resp = self._client.post(path, content=expr)
        if resp.status_code == 400:
            return {"ok": False, "error": _error_text(resp)}
        resp.raise_for_status()
        return {"ok": True, "value": resp.json()["value"]}

    def close(self) -> None:
        self._client.close()
