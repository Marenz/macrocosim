"""Site endpoint tests — address helpers, no process launched."""

from __future__ import annotations

from typing import Any

import pytest
from frequenz.quantities import Energy, Power

import macrocosim as mc
from macrocosim.handles import MicrogridExpect


def _site() -> mc.Site:
    return mc.connect(
        ui="127.0.0.1:8080",
        microgrids={
            1: mc.MicrogridEndpoint(id=1, name="a", grpc_addr="10.0.0.1:61000"),
            2: mc.MicrogridEndpoint(id=2, name="b", grpc_addr="10.0.0.2:61000"),
        },
    )


def test_grpc_is_first_microgrid_host_port() -> None:
    assert _site().grpc == "10.0.0.1:61000"


def test_no_microgrids_raises_a_clear_error() -> None:
    # connect() without microgrids must not leak a bare StopIteration
    # from the default-microgrid lookup.
    site = mc.connect(ui="127.0.0.1:8080")
    with pytest.raises(RuntimeError, match="no microgrid endpoints"):
        _ = site.grpc
    with pytest.raises(RuntimeError, match="no microgrid endpoints"):
        site.grpc_client()


def test_grpc_url_prefixes_scheme() -> None:
    assert _site().grpc_url == "grpc://10.0.0.1:61000"


def test_microgrid_grpc_url_selects_by_id() -> None:
    site = _site()
    assert site.microgrid_grpc_url(2) == "grpc://10.0.0.2:61000"


def test_resolve_mg_defaults_to_the_lowest_id() -> None:
    site = mc.connect(
        ui="127.0.0.1:8080",
        microgrids={
            7: mc.MicrogridEndpoint(id=7, name="a", grpc_addr="10.0.0.7:61000"),
            3: mc.MicrogridEndpoint(id=3, name="b", grpc_addr="10.0.0.3:61000"),
        },
    )
    assert site._resolve_microgrid_id(None) == 3
    assert site._resolve_microgrid_id(7) == 7


def test_setpoint_writes_refuse_bare_numbers() -> None:
    site = _site()
    with pytest.raises(TypeError, match="power must be Power"):
        site.set_active_power(6, 5000)  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="lifetime must be timedelta"):
        site.set_active_power(6, Power.from_watts(1), lifetime=5)  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="lower must be Power"):
        site.augment_bounds(6, -1000, Power.from_watts(1))  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="upper must be Power"):
        site.augment_bounds(6, Power.from_watts(-1), 1000)  # type: ignore[arg-type]


def test_microgrid_id_is_the_microgrid_argument() -> None:
    site = _site()
    site._http.control = lambda path, payload: {}  # type: ignore[method-assign]
    site.control_component(6, "drive", {"power_w": 1.0}, microgrid_id=2)
    old = {"mg" + "_id": 1}
    with pytest.raises(TypeError):
        site.components(**old)  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        site.control_component(6, "drive", {}, **old)  # type: ignore[arg-type]


def test_control_routes_to_the_default_microgrid() -> None:
    site = _site()
    calls: list[tuple[str, Any]] = []

    def fake_control(path: str, payload: Any) -> Any:
        calls.append((path, payload))
        return {}

    site._http.control = fake_control  # type: ignore[method-assign]
    site.control_component(6, "drive", {"power_w": 1.0})
    site.control_component(6, "status", {"health": "ok"}, microgrid_id=2)
    assert calls == [
        ("/api/mg/1/component/6/drive", {"power_w": 1.0}),
        ("/api/mg/2/component/6/status", {"health": "ok"}),
    ]


class _EnergyStub:
    """A stand-in site returning a fixed grid energy."""

    def __init__(self, wh: float) -> None:
        self._e = Energy.from_watt_hours(wh)

    def grid_energy(self, _microgrid_id: int | None = None) -> Energy:
        return self._e


async def test_expect_grid_energy_passes_within_matcher() -> None:
    expect = MicrogridExpect(_EnergyStub(66.0), None)  # type: ignore[arg-type]
    assert await expect.grid_energy(max=Energy.from_watt_hours(100)) == (
        Energy.from_watt_hours(66.0)
    )


async def test_expect_grid_energy_raises_when_over_cap() -> None:
    expect = MicrogridExpect(_EnergyStub(150.0), None)  # type: ignore[arg-type]
    with pytest.raises(AssertionError):
        await expect.grid_energy(max=Energy.from_watt_hours(100))


class _EnergySite:
    """A stand-in site returning a distinct fixed energy per stream, so a
    test can tell whether an expect reads the stream it names."""

    def __init__(self, **wh: float) -> None:
        self._wh = {k: Energy.from_watt_hours(v) for k, v in wh.items()}

    def grid_energy(self, _microgrid_id: int | None = None) -> Energy | None:
        return self._wh.get("grid")

    def consumer_energy(self, _microgrid_id: int | None = None) -> Energy | None:
        return self._wh.get("consumer")

    def pv_energy(self, _microgrid_id: int | None = None) -> Energy | None:
        return self._wh.get("pv")

    def battery_energy(self, _microgrid_id: int | None = None) -> Energy | None:
        return self._wh.get("battery")


async def test_expect_consumer_energy_reads_its_own_stream() -> None:
    # grid carries a large value; a `max` on consumer must read consumer,
    # not spill over onto grid.
    expect = MicrogridExpect(_EnergySite(grid=9999.0, consumer=50.0), None)  # type: ignore[arg-type]
    assert await expect.consumer_energy(max=Energy.from_watt_hours(100)) == (
        Energy.from_watt_hours(50.0)
    )


async def test_expect_pv_energy_within_matcher_negative() -> None:
    # PV production is signed negative.
    expect = MicrogridExpect(_EnergySite(pv=-30.0), None)  # type: ignore[arg-type]
    assert await expect.pv_energy(
        within=(Energy.from_watt_hours(-40), Energy.from_watt_hours(-20))
    ) == Energy.from_watt_hours(-30.0)


async def test_expect_battery_energy_approx_matcher() -> None:
    expect = MicrogridExpect(_EnergySite(battery=-8000.0), None)  # type: ignore[arg-type]
    assert await expect.battery_energy(
        approx=Energy.from_kilowatt_hours(-8), tol=Energy.from_watt_hours(100)
    ) == Energy.from_watt_hours(-8000.0)


async def test_expect_battery_energy_raises_under_floor() -> None:
    expect = MicrogridExpect(_EnergySite(battery=-50.0), None)  # type: ignore[arg-type]
    with pytest.raises(AssertionError):
        await expect.battery_energy(min=Energy.from_watt_hours(0))


def test_energy_metric_renders_to_lisp_symbol() -> None:
    assert mc.to_lisp_atom(mc.Metric.ENERGY) == "'energy"
