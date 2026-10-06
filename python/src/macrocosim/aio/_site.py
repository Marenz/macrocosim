"""The async-native ``Site``: every read, write, and wait is a coroutine.

Everything runs on the caller's event loop — no background loop thread,
no worker threads, no locks. This is the v2 core
(``docs/python-api-redesign.org``); the sync client remains available for
REPL use and simple scripts.

The surface is *signals*: every observable quantity is an object with
``read`` / ``expect`` / (where the simulator allows it) ``set``::

    async with mc.aio.launch(topology) as site:
        await load.power.set(Power.from_kilowatts(20))
        await site.grid_power.expect(mc.at_most(Power.from_kilowatts(13)))
        await site.battery_energy.expect(
            mc.at_most(Energy.from_watt_hours(-1)))

Component signals live on the topology builders (bound at launch); the
site's aggregates are signal properties here. A signal's *kind* picks
the check semantics (settle vs one-shot), and a new metric is a catalog
entry, not a set of new methods.
"""

from __future__ import annotations

import asyncio
import functools
import json
import shutil
import subprocess
import time
from collections.abc import AsyncIterator, Awaitable, Callable
from contextlib import asynccontextmanager
from datetime import timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any, TypeVar, cast

from frequenz.quantities import (
    Current,
    Energy,
    Percentage,
    Power,
    Quantity,
    ReactivePower,
)

from .. import metrics as _M
from .._http import EvalResult, control_path, scenario_path
from .._process import spawn_macrocosim, terminate
from ..build import RawLisp, _require, drive_parts, plug_ev_form
from ..enums import EvPreset
from ..errors import EvalRejected
from ..metrics import MetricSpec
from ..runtime import MicrogridEndpoint
from ..scenarios import _assert_report_passed, _NotActiveError
from ..signals import CumulativeSignal, Signal
from ._grpc import AsyncGrpcClient
from ._http import AsyncHttpClient

if TYPE_CHECKING:
    import os

    from .._grpc import ComponentInfo
    from ..build import Component, LaunchConfig
    from ..enums import CommandMode, Health, TelemetryMode
    from ..scenarios import JournalEvent, Scenario, ScenarioReport

Q = TypeVar("Q", bound=Quantity)
T = TypeVar("T")

# Aggregate metric name -> the formula stream serving it, for the names
# that differ; everything else streams under its metric name.
_STREAM_FOR = {
    "battery_power": "battery_pool_power",
    "battery_energy": "battery_pool_energy",
}

# Quantity type -> constructor from the wire float (W / Wh / %).
_FROM_WIRE: dict[type[Quantity], Callable[[float], Quantity]] = {
    Power: Power.from_watts,
    Energy: Energy.from_watt_hours,
    Percentage: Percentage.from_percent,
}


class Site:
    """Async handle onto a running macrocosim.

    Mirrors the sync ``macrocosim.Site`` surface, with every read, write,
    and wait a coroutine on the caller's loop.
    """

    def __init__(
        self,
        *,
        ui: str,
        microgrids: dict[int, MicrogridEndpoint],
        assets: str | None = None,
        dispatch: str | None = None,
        process: subprocess.Popen[bytes] | None = None,
        tmpdir: Path | None = None,
    ) -> None:
        self.ui = ui
        self.microgrids = microgrids
        self.assets = assets
        self.dispatch = dispatch
        self._process = process
        self._tmpdir = tmpdir
        self._http = AsyncHttpClient(f"http://{ui}")
        # Single-loop by design: creation is not racy, no lock needed.
        self._grpc_clients: dict[int, AsyncGrpcClient] = {}

    # --- endpoints -----------------------------------------------------------

    @property
    def grpc(self) -> str:
        """``host:port`` of the first (default) microgrid's gRPC API."""
        return self.microgrids[self._resolve_microgrid_id(None)].grpc_addr

    @property
    def grpc_url(self) -> str:
        """The default microgrid's gRPC API as a ``grpc://host:port`` URL."""
        return f"grpc://{self.grpc}"

    def microgrid_grpc_url(self, microgrid_id: int) -> str:
        """A specific microgrid's gRPC API as a ``grpc://host:port`` URL."""
        return f"grpc://{self.microgrids[microgrid_id].grpc_addr}"

    def _resolve_microgrid_id(self, microgrid_id: int | None) -> int:
        """The microgrid a per-microgrid call acts on: ``microgrid_id``,
        else the lowest id in :attr:`microgrids`."""
        if microgrid_id is not None:
            return microgrid_id
        if not self.microgrids:
            raise RuntimeError(
                "this Site has no microgrid endpoints; launch() discovers them, "
                "connect() needs microgrids={id: MicrogridEndpoint(...)}"
            )
        return min(self.microgrids)

    def _grpc(self, microgrid_id: int | None = None) -> AsyncGrpcClient:
        mg = self._resolve_microgrid_id(microgrid_id)
        client = self._grpc_clients.get(mg)
        if client is None:
            client = AsyncGrpcClient(self.microgrid_grpc_url(mg))
            self._grpc_clients[mg] = client
        return client

    # --- reads: component-level (gRPC) ---------------------------------------

    async def components(self, microgrid_id: int | None = None) -> list[ComponentInfo]:
        """List the microgrid's components (id, category, name)."""
        return await self._grpc(microgrid_id).components()

    async def active_power(
        self, component_id: int, microgrid_id: int | None = None
    ) -> Power | None:
        """A component's active power — one sample off its gRPC stream."""
        watts = await self._grpc(microgrid_id).active_power(component_id)
        return None if watts is None else Power.from_watts(watts)

    async def reactive_power(
        self, component_id: int, microgrid_id: int | None = None
    ) -> ReactivePower | None:
        """A component's reactive power — one sample off its gRPC stream."""
        vars_ = await self._grpc(microgrid_id).reactive_power(component_id)
        return None if vars_ is None else ReactivePower.from_volt_amperes_reactive(vars_)

    async def soc(
        self, component_id: int, microgrid_id: int | None = None
    ) -> Percentage | None:
        """A battery's state of charge — one sample off its gRPC stream."""
        pct = await self._grpc(microgrid_id).soc(component_id)
        return None if pct is None else Percentage.from_percent(pct)

    # --- reads: microgrid-level formula aggregates (HTTP) --------------------

    async def latest(self, microgrid_id: int | None = None) -> dict[str, Any]:
        """Latest sample per formula/component stream, keyed by name."""
        mg = self._resolve_microgrid_id(microgrid_id)
        return await self._http.get_json(f"/api/mg/{mg}/metrics/latest")

    async def formula(self, name: str, microgrid_id: int | None = None) -> float | None:
        """Raw value of one formula stream (e.g. ``"grid_power"``), or None."""
        snap = (await self.latest(microgrid_id)).get(name)
        return None if snap is None else snap.get("value")

    async def metric_value(
        self, spec: MetricSpec[Q], microgrid_id: int | None = None
    ) -> Q | None:
        """Read one *aggregate* metric from the catalog, typed by its spec."""
        value = await self.formula(_STREAM_FOR.get(spec.name, spec.name), microgrid_id)
        if value is None:
            return None
        return cast("Q", _FROM_WIRE[spec.quantity](value))

    # --- aggregate signals (flows; each *_energy integrates its *_power) ----

    def microgrid(self, microgrid_id: int | None = None) -> MicrogridSignals:
        """The aggregate signals of one microgrid.

        ``None`` (the default) is the first microgrid — the same signals
        the properties below expose directly.
        """
        return MicrogridSignals(self, microgrid_id)

    @property
    def grid_power(self) -> Signal[Power]:
        """Net power at the grid connection point (import positive)."""
        return self.microgrid().grid_power

    @property
    def pv_power(self) -> Signal[Power]:
        """Aggregate PV power (production negative)."""
        return self.microgrid().pv_power

    @property
    def consumer_power(self) -> Signal[Power]:
        """Aggregate consumer (load) power."""
        return self.microgrid().consumer_power

    @property
    def battery_power(self) -> Signal[Power]:
        """Aggregate battery-pool power (discharge negative)."""
        return self.microgrid().battery_power

    @property
    def grid_energy(self) -> CumulativeSignal[Energy]:
        """Cumulative net grid energy this run (the integral of grid_power)."""
        return self.microgrid().grid_energy

    @property
    def consumer_energy(self) -> CumulativeSignal[Energy]:
        """Cumulative consumer energy this run (integral of consumer_power)."""
        return self.microgrid().consumer_energy

    @property
    def pv_energy(self) -> CumulativeSignal[Energy]:
        """Cumulative PV energy this run (the integral of pv_power)."""
        return self.microgrid().pv_energy

    @property
    def battery_energy(self) -> CumulativeSignal[Energy]:
        """Cumulative battery-pool *flow* this run (integral of
        battery_power) — for the energy currently stored in a battery,
        read that battery's ``stored_energy`` signal."""
        return self.microgrid().battery_energy

    # --- component handles ----------------------------------------------------

    def component(
        self, target: Component | int, microgrid_id: int | None = None
    ) -> ComponentHandle:
        """A handle onto one component (``site[id]`` is the same).

        A builder bound to a non-default microgrid at launch carries
        its microgrid into the handle; an explicit ``microgrid_id`` wins.
        """
        from ..build import Component

        if isinstance(target, Component):
            if microgrid_id is None:
                microgrid_id = target._microgrid_id
            return ComponentHandle(self, target.id, microgrid_id)
        return ComponentHandle(self, int(target), microgrid_id)

    def __getitem__(self, target: Component | int) -> ComponentHandle:
        return self.component(target)

    # --- writes: setpoints + bounds (gRPC — the real gateway) -----------------

    async def set_active_power(
        self,
        component_id: int,
        power: Power,
        *,
        lifetime: timedelta | None = None,
        microgrid_id: int | None = None,
    ) -> None:
        """Command an active-power setpoint; rejections raise, as production."""
        _require(power, Power, "power")
        if lifetime is not None:
            _require(lifetime, timedelta, "lifetime")
        lifetime_s = lifetime.total_seconds() if lifetime is not None else None
        await self._grpc(microgrid_id).set_active_power(
            component_id, power.as_watts(), lifetime_s=lifetime_s
        )

    async def augment_bounds(
        self,
        component_id: int,
        lower: Power,
        upper: Power,
        microgrid_id: int | None = None,
    ) -> None:
        """Narrow a component's effective active-power bounds (TTL-limited)."""
        _require(lower, Power, "lower")
        _require(upper, Power, "upper")
        await self._grpc(microgrid_id).augment_active_power_bounds(
            component_id, lower.as_watts(), upper.as_watts()
        )

    # --- eval (the escape hatch) -----------------------------------------------

    async def eval(self, expr: str, microgrid_id: int | None = None) -> EvalResult:
        """Evaluate a raw Lisp form on the running interpreter."""
        return await self._http.eval(expr, microgrid_id)

    async def _eval_ok(self, expr: str, microgrid_id: int | None = None) -> EvalResult:
        """Eval and raise :class:`EvalRejected` on an interpreter rejection.

        The one choke point for programmatic eval — rejections can never
        silently no-op.
        """
        result = await self.eval(expr, microgrid_id)
        if not result.get("ok", True):
            raise EvalRejected(f"eval of {expr!r} failed: {result.get('error')}")
        return result

    # --- EV chargers: plug state (Lisp-backed, read over HTTP) -----------

    async def plug_ev(
        self,
        component_id: int,
        preset: EvPreset | str,
        *,
        soc: Percentage | None = None,
        target_soc: Percentage | None = None,
        phases: int | None = None,
        max_current: Current | None = None,
        capacity: Energy | None = None,
        taper_start: Percentage | None = None,
        taper_floor: Percentage | None = None,
        microgrid_id: int | None = None,
    ) -> None:
        """Plug a preset car into charger ``component_id``."""
        await self._eval_ok(
            plug_ev_form(
                component_id,
                preset,
                soc=soc,
                target_soc=target_soc,
                phases=phases,
                max_current=max_current,
                capacity=capacity,
                taper_start=taper_start,
                taper_floor=taper_floor,
            ),
            self._resolve_microgrid_id(microgrid_id),
        )

    async def unplug_ev(self, component_id: int, microgrid_id: int | None = None) -> bool:
        """Unplug the car; False when the charger was already empty."""
        mg = self._resolve_microgrid_id(microgrid_id)
        result = await self._eval_ok(f"(unplug-ev {component_id})", mg)
        return result.get("value") == "t"

    async def ev_info(
        self, component_id: int, microgrid_id: int | None = None
    ) -> dict[str, Any]:
        """The car plugged into ``component_id``: ``{"plugged": False}`` or its
        fields, plus ``presets`` (the catalog) either way."""
        mg = self._resolve_microgrid_id(microgrid_id)
        return await self._http.get_json(f"/api/mg/{mg}/component/{component_id}/ev")

    async def control_component(
        self,
        component_id: int,
        action: str,
        payload: dict[str, Any],
        microgrid_id: int | None = None,
    ) -> None:
        """POST a typed control request (``status`` / ``drive``) for a component.

        Rejections (unknown id, bad value) raise ``ControlRejected``.
        """
        path = control_path(
            component_id, action, self._resolve_microgrid_id(microgrid_id)
        )
        await self._http.control(path, payload)

    # --- scenarios --------------------------------------------------------------

    def scenario(self, name: str) -> ScenarioRun:
        """Handle onto a registered ``(define-scenario …)`` for run/report."""
        return ScenarioRun(self, name)

    async def define_scenario(self, scenario: Scenario) -> ScenarioRun:
        """Register a Python-authored ``Scenario`` and return its ScenarioRun."""
        await self._eval_ok(scenario.to_lisp())
        return ScenarioRun(self, scenario.name)

    # --- generic polling ----------------------------------------------------------

    async def read_until(
        self,
        read: Callable[[], Awaitable[T]],
        predicate: Callable[[T], bool],
        *,
        timeout: timedelta = timedelta(seconds=10),
        poll: timedelta = timedelta(milliseconds=250),
    ) -> T:
        """Await ``read()`` repeatedly until ``predicate`` holds or timeout.

        Returns the last observed value either way (the caller asserts on it).
        """
        deadline = time.monotonic() + timeout.total_seconds()
        interval = poll.total_seconds()
        value = await read()
        while not predicate(value) and time.monotonic() < deadline:
            await asyncio.sleep(interval)
            value = await read()
        return value

    # --- lifecycle ------------------------------------------------------------------

    async def aclose(self) -> None:
        """Close the transports and stop a launched process."""
        for client in self._grpc_clients.values():
            await client.aclose()
        self._grpc_clients.clear()
        await self._http.aclose()
        terminate(self._process)
        # The launch tmpdir (rendered config + endpoints + log) is only
        # debris once its process is gone. A launch that fails before
        # the handshake never builds a Site, so its log survives for
        # post-mortem reading (a post-handshake bind failure does tear
        # the Site down — that is a client-side error whose diagnosis
        # doesn't need the sim log).
        if self._tmpdir is not None:
            shutil.rmtree(self._tmpdir, ignore_errors=True)
            self._tmpdir = None

    async def __aenter__(self) -> Site:
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.aclose()


class MicrogridSignals:
    """One microgrid's aggregate signals (flows over the whole graph)."""

    def __init__(self, site: Site, microgrid_id: int | None) -> None:
        self._site = site
        self._microgrid_id = microgrid_id

    def _signal(self, spec: MetricSpec[Q]) -> Signal[Q]:
        read = functools.partial(self._site.metric_value, spec, self._microgrid_id)
        return Signal(spec, read, spec.name)

    def _cumulative(self, spec: MetricSpec[Q]) -> CumulativeSignal[Q]:
        read = functools.partial(self._site.metric_value, spec, self._microgrid_id)
        return CumulativeSignal(spec, read, spec.name)

    @property
    def grid_power(self) -> Signal[Power]:
        """Net power at the grid connection point (import positive)."""
        return self._signal(_M.GRID_POWER)

    @property
    def pv_power(self) -> Signal[Power]:
        """Aggregate PV power (production negative)."""
        return self._signal(_M.PV_POWER)

    @property
    def consumer_power(self) -> Signal[Power]:
        """Aggregate consumer (load) power."""
        return self._signal(_M.CONSUMER_POWER)

    @property
    def battery_power(self) -> Signal[Power]:
        """Aggregate battery-pool power (discharge negative)."""
        return self._signal(_M.BATTERY_POWER)

    @property
    def grid_energy(self) -> CumulativeSignal[Energy]:
        """Cumulative net grid energy this run (the integral of grid_power)."""
        return self._cumulative(_M.GRID_ENERGY)

    @property
    def consumer_energy(self) -> CumulativeSignal[Energy]:
        """Cumulative consumer energy this run (integral of consumer_power)."""
        return self._cumulative(_M.CONSUMER_ENERGY)

    @property
    def pv_energy(self) -> CumulativeSignal[Energy]:
        """Cumulative PV energy this run (the integral of pv_power)."""
        return self._cumulative(_M.PV_ENERGY)

    @property
    def battery_energy(self) -> CumulativeSignal[Energy]:
        """Cumulative battery-pool *flow* this run (integral of
        battery_power) — for the energy currently stored in a battery,
        read that battery's ``stored_energy`` signal."""
        return self._cumulative(_M.BATTERY_ENERGY)


class ComponentHandle:
    """A component in a running site, acted on by intent (async)."""

    def __init__(
        self, site: Site, component_id: int, microgrid_id: int | None = None
    ) -> None:
        self._site = site
        self._id = component_id
        self._microgrid_id = microgrid_id

    async def command(
        self,
        *,
        active_power: Power | None = None,
        bounds: tuple[Power, Power] | None = None,
        lifetime: timedelta | None = None,
    ) -> None:
        """Issue a control command as the app would — through the gRPC gateway."""
        if active_power is not None:
            await self._site.set_active_power(
                self._id, active_power, lifetime=lifetime, microgrid_id=self._microgrid_id
            )
        if bounds is not None:
            await self._site.augment_bounds(
                self._id, bounds[0], bounds[1], microgrid_id=self._microgrid_id
            )

    async def status(
        self,
        *,
        health: Health | None = None,
        command_mode: CommandMode | None = None,
        telemetry_mode: TelemetryMode | None = None,
    ) -> None:
        """Set the component's operational status (fault injection).

        Goes over the typed control API; an unknown component or a bad
        value raises ``ControlRejected``.
        """
        payload: dict[str, str] = {}
        if health is not None:
            payload["health"] = health.value
        if command_mode is not None:
            payload["command_mode"] = command_mode.value
        if telemetry_mode is not None:
            payload["telemetry_mode"] = telemetry_mode.value
        if payload:
            await self._site.control_component(
                self._id, "status", payload, self._microgrid_id
            )

    async def drive(
        self,
        *,
        power: Power | RawLisp | None = None,
        sunlight: Percentage | None = None,
    ) -> None:
        """Drive the environment: a meter's published power, a PV's sunlight.

        Constant values go over the typed control API (rejections raise
        ``ControlRejected``); a ``RawLisp`` power (a lambda or symbol,
        re-resolved every tick) goes through the microgrid's eval. A bare
        number raises :class:`TypeError`.
        """
        form, payload = drive_parts(self._id, power, sunlight)
        if form is not None:
            await self._site._eval_ok(
                form, self._site._resolve_microgrid_id(self._microgrid_id)
            )
        if payload:
            await self._site.control_component(
                self._id, "drive", payload, self._microgrid_id
            )

    @property
    def power(self) -> Signal[Power]:
        """The component's active power (gRPC) — read/expect only.

        The category-typed builders are the richer surface (a Meter's
        ``power`` is also settable); this raw-id handle cannot know the
        category, so its signals only observe.
        """
        site, cid, mg = self._site, self._id, self._microgrid_id

        async def read() -> Power | None:
            return await site.active_power(cid, mg)

        return Signal(_M.ACTIVE_POWER, read, f"component {cid} active_power")

    @property
    def soc(self) -> Signal[Percentage]:
        """The battery's state of charge (gRPC) — read/expect only."""
        site, cid, mg = self._site, self._id, self._microgrid_id

        async def read() -> Percentage | None:
            return await site.soc(cid, mg)

        return Signal(_M.SOC, read, f"component {cid} soc")


class ScenarioRun:
    """A registered scenario bound to a running site (async)."""

    def __init__(self, site: Site, name: str) -> None:
        self._site = site
        self._name = name
        # The microgrid run(wait=True) picked; None until one did.
        self._microgrid_id: int | None = None

    async def _length_s(self) -> float | None:
        for scenario in await self._site._http.get_json("/api/scenarios"):
            if scenario.get("name") == self._name:
                return scenario.get("length_s")
        return None

    async def _wait_length(self, until: timedelta | None) -> float:
        length = until.total_seconds() if until is not None else await self._length_s()
        if length is None:
            raise ValueError(
                f"scenario {self._name!r} has no :length; pass until= to bound the wait"
            )
        return length

    def _run_mg(self) -> int:
        """The microgrid run() picked, else the lowest one."""
        return (
            self._microgrid_id
            if self._microgrid_id is not None
            else self._site._resolve_microgrid_id(None)
        )

    async def run(
        self,
        *,
        wait: bool = True,
        until: timedelta | None = None,
        poll: timedelta = timedelta(seconds=1),
    ) -> ScenarioRun:
        """Start the scenario; with ``wait`` block until it finishes, stop it."""
        start = f"/api/scenarios/{self._name}/start"
        if not wait:
            await self._site._http.post(start)
            return self
        # Resolve the wait length and the microgrid BEFORE starting,
        # so an unwaitable scenario fails fast instead of being left
        # running with nothing to stop it. One read of the
        # microgrid's journal makes a microgrid the server does not
        # have fail here.
        length = await self._wait_length(until)
        mg = self._microgrid_id = self._site._resolve_microgrid_id(None)
        await self._site._http.get_json(scenario_path(mg))
        await self._site._http.post(start)
        return await self._wait_for(length, poll, mg)

    async def wait(
        self,
        *,
        until: timedelta | None = None,
        poll: timedelta = timedelta(seconds=1),
    ) -> ScenarioRun:
        """Block until the running scenario finishes, then stop it.

        The companion to ``run(wait=False)`` — start the scenario, do
        other work (boot an app under test), then wait for the report.
        """
        length = await self._wait_length(until)
        return await self._wait_for(length, poll, self._run_mg())

    def _assert_active(self, state: ScenarioReport) -> None:
        # The server tracks one scenario; a mismatched (or absent) name
        # means this run never started or another scenario's state is
        # live — waiting on it, or judging its report, would silently
        # test the wrong thing.
        if state.get("name") != self._name:
            raise _NotActiveError(self._name, state.get("name"))

    async def _wait_for(self, length: float, poll: timedelta, mg: int) -> ScenarioRun:
        deadline = time.monotonic() + length + 5.0
        interval = poll.total_seconds()
        try:
            while time.monotonic() < deadline:
                state = await self._site._http.get_json(scenario_path(mg))
                self._assert_active(state)
                if state.get("ended_at") is not None:
                    break
                if (state.get("elapsed_s") or 0.0) >= length:
                    break
                await asyncio.sleep(interval)
        except _NotActiveError:
            # The stop route takes no name, so posting it here would
            # stop the scenario that is active instead.
            raise
        except BaseException as err:
            # Any other failed wait still posts the stop, so no
            # scenario is left running; the wait's error is the one
            # raised.
            await self._stop(err)
            raise
        await self._stop()
        return self

    async def _stop(self, err: BaseException | None = None) -> None:
        # Posts the stop. With `err`, the wait's error, a failed stop
        # is added to `err` as a note instead of raised.
        try:
            await self._site._http.post("/api/scenarios/stop")
        except Exception as stop_err:
            if err is None:
                raise
            err.add_note(f"stopping scenario {self._name!r} failed: {stop_err!r}")

    async def report(self) -> ScenarioReport:
        """The parsed scenario report (pass/fail ledger + peak/soc stats)."""
        # The report carries the scenario name it belongs to; checking
        # it in the same response avoids a two-request race.
        report = await self._site._http.get_json(scenario_path(self._run_mg(), "/report"))
        self._assert_active(report)
        return report

    async def assert_passed(self) -> ScenarioReport:
        """Raise if any ``(check …)`` failed, or if none ran; return the report
        otherwise."""
        report = await self.report()
        _assert_report_passed(self._name, report)
        return report

    async def events(self, *, since: int = 0) -> list[JournalEvent]:
        """The scenario's journal events (list of ``{kind, payload, …}``)."""
        body = await self._site._http.get_json(
            scenario_path(self._run_mg(), f"/events?since={since}")
        )
        return body.get("events", [])


def _components_of(config: Any) -> list[tuple[Any, int | None]]:
    """Every builder Component in a launch config, with its microgrid id.

    Components under a single top-level ``Microgrid`` keep ``None`` (the
    site default routes there anyway); in a multi-microgrid config each
    component carries its owning ``Microgrid``'s id, so its signals
    route to the right one.
    """
    from ..build import Component, Microgrid

    microgrids = 0

    def count(node: Any) -> None:
        nonlocal microgrids
        if isinstance(node, Microgrid):
            microgrids += 1
        elif isinstance(node, (list, tuple)):
            for item in node:
                count(item)

    count(config)
    out: list[tuple[Any, int | None]] = []

    def walk(node: Any, microgrid_id: int | None) -> None:
        if isinstance(node, Component):
            out.append((node, microgrid_id))
            for child in node.successors:
                walk(child, microgrid_id)
        elif isinstance(node, Microgrid):
            walk(node.topology, node.id if microgrids > 1 else None)
        elif isinstance(node, (list, tuple)):
            for item in node:
                walk(item, microgrid_id)

    walk(config, None)
    return out


def connect(
    *,
    ui: str,
    microgrids: dict[int, MicrogridEndpoint] | None = None,
    assets: str | None = None,
    dispatch: str | None = None,
) -> Site:
    """Attach to an already-running macrocosim (no process is spawned)."""
    return Site(
        ui=ui,
        microgrids=microgrids or {},
        assets=assets,
        dispatch=dispatch,
        process=None,
    )


@asynccontextmanager
async def launch(
    config: LaunchConfig,
    *,
    bin: str | os.PathLike[str] | None = None,
    ready_timeout: timedelta = timedelta(seconds=20),
) -> AsyncIterator[Site]:
    """Boot macrocosim on ephemeral ports and yield a ready async ``Site``.

    Same contract as the sync ``macrocosim.launch``, but the handshake wait
    awaits instead of blocking, and the site is torn down on exit::

        async with mc.aio.launch(topology) as site:
            ...
    """
    spawned = spawn_macrocosim(config, bin)
    site: Site | None = None
    try:
        deadline = time.monotonic() + ready_timeout.total_seconds()
        while True:
            if (
                spawned.endpoints_file.exists()
                and spawned.endpoints_file.stat().st_size > 0
            ):
                break
            if spawned.process.poll() is not None:
                spawned.fail(
                    RuntimeError,
                    f"macrocosim exited early (code {spawned.process.returncode}) "
                    f"before emitting endpoints:",
                )
            if time.monotonic() >= deadline:
                spawned.fail(
                    TimeoutError,
                    f"macrocosim did not emit endpoints within {ready_timeout}:",
                )
            await asyncio.sleep(0.1)
        endpoints = json.loads(spawned.endpoints_file.read_text())
        # Mirror the sync launch guard: the binary reports ready even
        # with an empty registry (the interactive bare-engine state),
        # but a client's config was supposed to build a topology.
        # fail() attaches the log tail.
        if not endpoints.get("microgrids"):
            spawned.fail(
                RuntimeError,
                "macrocosim booted but the config registered no microgrids "
                "— does it call (make-microgrid …)?",
            )
        microgrids = {
            int(m["id"]): MicrogridEndpoint(
                id=int(m["id"]), name=m["name"], grpc_addr=m["grpc_addr"]
            )
            for m in endpoints.get("microgrids", [])
        }
        site = Site(
            ui=endpoints["ui"],
            microgrids=microgrids,
            assets=endpoints.get("assets"),
            dispatch=endpoints.get("dispatch"),
            process=spawned.process,
            tmpdir=spawned.tmpdir,
        )
        # Bind the topology objects: from here on, the builders are the
        # live handles (LOAD.power.set(...), BAT.soc.read(), ...).
        components = _components_of(config)
        bound: list[Any] = []
        try:
            for component, microgrid_id in components:
                component._bind(site, microgrid_id)
                bound.append(component)
            yield site
        finally:
            # Unbind whatever got bound — also on a failure partway
            # through the bind loop, so a retry starts clean.
            for component in bound:
                component._unbind()
    finally:
        if site is not None:
            await site.aclose()
        else:
            terminate(spawned.process)
