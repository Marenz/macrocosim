"""Async client over macrocosim's HTTP ``/api/*`` control plane.

Internal: users reach these calls through :class:`macrocosim.aio.Site`.
Each method maps to one endpoint and returns parsed JSON.
"""

from __future__ import annotations

from typing import Any

import httpx

from .._http import EvalResult, _error_text
from ..errors import ControlRejected


class AsyncHttpClient:
    """Async ``httpx`` client bound to a macrocosim UI server."""

    def __init__(self, base_url: str, *, timeout: float = 10.0) -> None:
        self._client = httpx.AsyncClient(base_url=base_url, timeout=timeout)

    async def get_json(self, path: str) -> Any:
        """GET ``path`` and return the parsed JSON (shape is endpoint-specific)."""
        resp = await self._client.get(path)
        resp.raise_for_status()
        return resp.json()

    async def post(self, path: str, content: str = "") -> Any:
        """POST a raw body to ``path``; return parsed JSON (or ``{}``)."""
        resp = await self._client.post(path, content=content)
        resp.raise_for_status()
        return resp.json() if resp.content else {}

    async def control(self, path: str, payload: Any) -> Any:
        """POST a typed control request; a 4xx rejection raises.

        The control endpoints report rejections as structured JSON
        (``{"error": ...}``) with a 400/404 status — turn that into
        :class:`ControlRejected` so a rejection can never silently no-op.
        """
        resp = await self._client.post(path, json=payload)
        if 400 <= resp.status_code < 500:
            raise ControlRejected(_error_text(resp))
        resp.raise_for_status()
        return resp.json() if resp.content else {}

    async def eval(self, expr: str, mg_id: int | None = None) -> EvalResult:
        """POST a Lisp form: the whole-site ``/api/eval`` without an
        ``mg_id``, the microgrid's eval with one."""
        path = "/api/eval" if mg_id is None else f"/api/mg/{mg_id}/eval"
        resp = await self._client.post(path, content=expr)
        if resp.status_code == 400:
            return {"ok": False, "error": _error_text(resp)}
        resp.raise_for_status()
        return {"ok": True, "value": resp.json()["value"]}

    async def aclose(self) -> None:
        """Close the underlying connection pool."""
        await self._client.aclose()
