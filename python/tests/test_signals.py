"""Signals and matchers: the typed verb surface on builders and sites."""

from __future__ import annotations

from datetime import timedelta
from typing import Any

import pytest
from frequenz.quantities import Energy, Percentage, Power

import macrocosim as mc
from macrocosim.errors import NoSample
from macrocosim.metrics import GRID_POWER
from macrocosim.signals import CumulativeSignal, Signal

kW = Power.from_kilowatts
percent = Percentage.from_percent


def test_matchers_carry_typed_fields() -> None:
    assert mc.at_most(kW(13)).max == kW(13)
    assert mc.at_least(kW(1)).min == kW(1)
    assert mc.between(kW(1), kW(2)).within == (kW(1), kW(2))
    near = mc.near(kW(20), tol=kW(1))
    assert (near.approx, near.tol) == (kW(20), kW(1))
    with pytest.raises(TypeError):
        mc.near(kW(20))  # type: ignore[call-arg]  # tol is required


async def test_signal_read_raises_no_sample() -> None:
    async def read() -> Power | None:
        return None

    signal = Signal(GRID_POWER, read, "grid_power")
    assert await signal.try_read() is None
    with pytest.raises(NoSample, match="grid_power"):
        await signal.read(wait=timedelta(seconds=0.1))


async def test_signal_read_returns_a_plain_quantity() -> None:
    reads = iter([None, kW(5)])

    async def read() -> Power | None:
        return next(reads, kW(5))

    signal = Signal(GRID_POWER, read, "grid_power")
    value = await signal.read(wait=timedelta(seconds=1))
    assert value == kW(5)  # no None in the type; availability was awaited


async def test_signal_expect_takes_one_matcher() -> None:
    async def read() -> Power | None:
        return kW(5)

    signal = Signal(GRID_POWER, read, "grid_power")
    value = await signal.expect(mc.at_most(kW(10)), timeout=timedelta(seconds=0.2))
    assert value == kW(5)
    with pytest.raises(AssertionError, match="grid_power"):
        await signal.expect(mc.at_most(kW(1)), timeout=timedelta(seconds=0.1))


def test_cumulative_expect_has_no_hold_for() -> None:
    # The kind distinction is the method signature, not a runtime branch.
    async def read() -> Energy | None:
        return Energy.from_watt_hours(1)

    signal = CumulativeSignal(mc.metrics.GRID_ENERGY, read, "grid_energy")
    with pytest.raises(TypeError, match="hold_for"):
        signal.expect(mc.at_most(Energy.zero()), hold_for=timedelta(seconds=1))  # type: ignore[call-arg]


class FakeAioSite:
    """The slice of the aio Site the component signals touch."""

    def __init__(self) -> None:
        self.controls: list[tuple[int, str, dict[str, Any]]] = []
        self.soc_pct = 42.0

    async def control_component(
        self, cid: int, action: str, payload: dict[str, Any], microgrid_id=None
    ) -> None:
        self.controls.append((cid, action, payload))

    async def active_power(self, cid: int, microgrid_id=None) -> Power | None:
        return Power.from_watts(float(cid))

    async def soc(self, cid: int, microgrid_id=None) -> Percentage | None:
        return percent(self.soc_pct)


def test_builders_return_typed_components() -> None:
    meter = mc.meter(id=5)
    bat = mc.battery(id=4, capacity=Energy.from_kilowatt_hours(100))
    inv = mc.battery_inverter(id=3, successors=[bat])
    pv = mc.solar_inverter(id=8)
    assert isinstance(meter, mc.build.Meter)
    assert isinstance(bat, mc.build.Battery)
    assert isinstance(inv, mc.build.BatteryInverter)
    assert isinstance(pv, mc.build.SolarInverter)
    # The Lisp rendering is untouched by the live-handle machinery.
    assert meter.to_lisp() == "(make-meter :id 5)"


async def test_unbound_component_signals_raise_on_use() -> None:
    # Properties never raise (scenarios need the identity pre-launch);
    # calling a verb on an unbound builder does.
    meter = mc.meter(id=5)
    signal = meter.power  # fine: identity only
    with pytest.raises(RuntimeError, match="not bound"):
        await signal.set(kW(1))
    with pytest.raises(ValueError, match="explicit id="):
        _ = mc.meter().id


def test_a_built_component_exposes_its_id() -> None:
    assert mc.meter(id=5).id == 5


async def test_idless_components_bind_but_have_no_signals() -> None:
    # A topology may carry id-less components (the server auto-assigns);
    # they launch fine — only using their signals raises.
    grid = mc.grid()
    site = FakeAioSite()
    grid._bind(site)
    with pytest.raises(ValueError, match="explicit id="):
        await grid.health.set(mc.Health.OK)
    grid._unbind()


async def test_bound_meter_power_reads_and_sets() -> None:
    site = FakeAioSite()
    meter = mc.meter(id=5)
    meter._bind(site)
    assert await meter.power.read() == Power.from_watts(5.0)
    await meter.power.set(kW(20))
    assert site.controls == [(5, "drive", {"power_w": 20000.0})]
    with pytest.raises(RuntimeError, match="already bound"):
        meter._bind(site)
    meter._unbind()
    with pytest.raises(RuntimeError, match="not bound"):
        await meter.power.read()


async def test_battery_soc_gets_and_sets() -> None:
    site = FakeAioSite()
    bat = mc.battery(id=4, capacity=Energy.from_kilowatt_hours(100))
    bat._bind(site)
    assert await bat.soc.read() == percent(42.0)
    await bat.soc.set(percent(11.0))
    assert site.controls == [(4, "drive", {"soc_pct": 11.0})]


async def test_stored_energy_is_state_not_flow() -> None:
    site = FakeAioSite()
    bat = mc.battery(id=4, capacity=Energy.from_kilowatt_hours(100))
    bat._bind(site)
    # 42 % of 100 kWh.
    assert await bat.stored_energy.read() == Energy.from_kilowatt_hours(42.0)
    # stored_energy is read/expect only — no .set.
    assert not hasattr(bat.stored_energy, "set")


async def test_health_and_sunlight_are_settings() -> None:
    site = FakeAioSite()
    inv = mc.battery_inverter(id=3)
    pv = mc.solar_inverter(id=8)
    inv._bind(site)
    pv._bind(site)
    await inv.health.set(mc.Health.ERROR)
    await pv.sunlight.set(percent(80.0))
    assert site.controls == [
        (3, "status", {"health": "error"}),
        (8, "drive", {"sunlight_pct": 80.0}),
    ]
    # The inverter's power is measured, never test-set: that is command()'s
    # job through the real gateway.
    assert not hasattr(inv.power, "set")
