"""Handle-layer tests — a fake site records what command/fault/drive emit."""

from __future__ import annotations

import asyncio
from datetime import timedelta
from typing import Any

import httpx
import pytest
from frequenz.quantities import Percentage, Power

from macrocosim._http import HttpClient
from macrocosim.aio._http import AsyncHttpClient
from macrocosim.build import raw
from macrocosim.enums import CommandMode, Health, TelemetryMode
from macrocosim.errors import ControlRejected
from macrocosim.handles import ComponentHandle


class FakeSite:
    def __init__(self) -> None:
        self.evals: list[str] = []
        self.controls: list[tuple[int, str, dict[str, Any]]] = []
        self.reject_controls: str | None = None
        self.setpoints: list[tuple[int, float, float | None]] = []
        self.bounds: list[tuple[int, float, float]] = []

    def eval(self, expr: str, microgrid_id: int | None = None) -> dict:
        self.evals.append(expr)
        return {"ok": True}

    def _resolve_microgrid_id(self, microgrid_id: int | None) -> int:
        return 1 if microgrid_id is None else microgrid_id

    def control_component(
        self, cid: int, action: str, payload: dict[str, Any], microgrid_id=None
    ) -> None:
        if self.reject_controls is not None:
            raise ControlRejected(self.reject_controls)
        self.controls.append((cid, action, payload))

    def set_active_power(
        self,
        cid: int,
        power: Power,
        *,
        lifetime: timedelta | None = None,
        microgrid_id=None,
    ) -> None:
        secs = None if lifetime is None else lifetime.total_seconds()
        self.setpoints.append((cid, power.as_watts(), secs))

    def augment_bounds(
        self, cid: int, lower: Power, upper: Power, microgrid_id: int | None = None
    ) -> None:
        self.bounds.append((cid, lower.as_watts(), upper.as_watts()))


def _h(site: FakeSite, cid: int = 3) -> ComponentHandle:
    return ComponentHandle(site, cid)


def test_status_posts_typed_payloads() -> None:
    w = FakeSite()
    _h(w).status(health=Health.ERROR)
    _h(w).status(command_mode=CommandMode.TIMEOUT, telemetry_mode=TelemetryMode.SILENT)
    assert w.controls == [
        (3, "status", {"health": "error"}),
        (3, "status", {"command_mode": "timeout", "telemetry_mode": "silent"}),
    ]
    assert w.evals == []  # stimuli no longer go through eval


def test_rejected_control_raises() -> None:
    # The control endpoints report rejections as structured errors — they
    # must raise, not silently no-op the stimulus.
    w = FakeSite()
    w.reject_controls = "component 3 not found"
    with pytest.raises(ControlRejected, match="not found"):
        _h(w).drive(power=Power.from_watts(100))
    with pytest.raises(ValueError, match="not found"):
        # Still catchable as the historic ValueError.
        _h(w).status(health=Health.ERROR)


def test_command_setpoint_and_bounds_go_grpc() -> None:
    w = FakeSite()
    _h(w).command(active_power=Power.from_kilowatts(2), lifetime=timedelta(seconds=30))
    _h(w).command(bounds=(Power.from_kilowatts(-1), Power.from_kilowatts(1)))
    assert w.setpoints == [(3, 2000.0, 30.0)]
    assert w.bounds == [(3, -1000.0, 1000.0)]
    assert w.controls == []  # gateway commands don't touch the control API


def test_drive_constants_go_typed_and_raw_goes_eval() -> None:
    w = FakeSite()
    _h(w, 6).drive(power=Power.from_megawatts(2))
    _h(w, 6).drive(power=Power.from_watts(-5000))
    # A dynamic source (lambda / symbol) still needs the Lisp escape hatch.
    _h(w, 6).drive(power=raw("(lambda () (+ 1000.0 (random 500)))"))
    assert w.controls == [
        (6, "drive", {"power_w": 2000000.0}),
        (6, "drive", {"power_w": -5000.0}),
    ]
    assert w.evals == [
        "(set-meter-power 6 (lambda () (+ 1000.0 (random 500))))",
    ]


def test_drive_sunlight() -> None:
    w = FakeSite()
    _h(w, 5).drive(sunlight=Percentage.from_percent(30))
    assert w.controls == [(5, "drive", {"sunlight_pct": 30.0})]


def test_handle_methods_chain() -> None:
    w = FakeSite()
    h = ComponentHandle(w, 3)
    assert h.status(health=Health.OK).command(active_power=Power.from_watts(0)) is h


def _client_answering(status: int, body: dict) -> HttpClient:
    client = HttpClient("http://macrocosim.test")
    client._client = httpx.Client(
        base_url="http://macrocosim.test",
        transport=httpx.MockTransport(lambda _req: httpx.Response(status, json=body)),
    )
    return client


def test_eval_maps_400_to_ok_false() -> None:
    bad = _client_answering(400, {"error": "boom"})
    assert bad.eval("(x)") == {"ok": False, "error": "boom"}
    good = _client_answering(200, {"value": "3"})
    assert good.eval("(+ 1 2)") == {"ok": True, "value": "3"}


def _no_content(_req: httpx.Request) -> httpx.Response:
    return httpx.Response(204)


def test_post_and_control_tolerate_204() -> None:
    client = HttpClient("http://macrocosim.test")
    client._client = httpx.Client(
        base_url="http://macrocosim.test",
        transport=httpx.MockTransport(_no_content),
    )
    assert client.post("/api/mg/1/component/2/status") == {}
    assert client.control("/api/mg/1/component/2/drive", {"power_w": 1.0}) == {}


def test_async_post_and_control_tolerate_204() -> None:
    async def run() -> None:
        client = AsyncHttpClient("http://macrocosim.test")
        client._client = httpx.AsyncClient(
            base_url="http://macrocosim.test",
            transport=httpx.MockTransport(_no_content),
        )
        assert await client.post("/api/scenarios/stop") == {}
        assert await client.control("/api/mg/1/component/2/drive", {}) == {}
        await client.aclose()

    asyncio.run(run())
