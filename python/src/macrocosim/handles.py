"""Component and microgrid handles — the object-oriented control surface.

``site.component(id)`` (or ``site[id]``) returns a :class:`ComponentHandle`
bound to the running site; act on it by *intent*:

    inv = site.component(3)
    inv.command(active_power=Power.from_kilowatts(2))  # what the app sends (gRPC)
    inv.status(health=Health.ERROR)                    # set operational status
    site[6].drive(power=Power.from_megawatts(2))       # drive the environment
    inv.active_power()                                 # read -> Power | None
    await inv.expect.active_power(                      # settle-aware assertion
        approx=Power.from_kilowatts(2), tol=Power.from_watts(300))

``command`` issues a control command through the real gRPC gateway (so an
out-of-envelope value raises :class:`~macrocosim.errors.SetpointRejected`, the
production behaviour under test); ``status`` / ``drive`` are test-side stimuli
POSTed to the typed control API
(``/api/mg/{mg}/component/{id}/status`` / ``…/drive``); only a
``RawLisp`` drive goes through the microgrid's eval. The ``expect``
assertions are ``async`` (they await between polls): ``await
site.expect.grid_power(...)`` (or ``site.microgrid(id)`` for a
non-default one).

Every ``expect`` method here is one-line sugar over the kind-aware engine
(:func:`macrocosim.assertions.expect_metric`): the metric's entry in
:mod:`macrocosim.metrics` decides whether it settles (power, SoC) or is
checked once as a running total (energy).
"""

from __future__ import annotations

from datetime import timedelta
from typing import TYPE_CHECKING, Any

from .assertions import expect_metric
from .build import RawLisp, to_lisp_atom
from .errors import EvalRejected
from .metrics import (
    ACTIVE_POWER,
    BATTERY_ENERGY,
    BATTERY_POWER,
    CONSUMER_ENERGY,
    CONSUMER_POWER,
    GRID_ENERGY,
    GRID_POWER,
    PV_ENERGY,
    PV_POWER,
    SOC,
)

if TYPE_CHECKING:
    from frequenz.quantities import Energy, Percentage, Power

    from .build import Component
    from .enums import CommandMode, Health, TelemetryMode
    from .runtime import Site

_TIMEOUT = timedelta(seconds=10)
_POLL = timedelta(milliseconds=250)


async def _expect(
    spec: Any,
    read: Any,
    label: str | None,
    *,
    approx: Any = None,
    tol: Any = None,
    within: Any = None,
    max: Any = None,  # noqa: A002 — mirrors the engine's matcher names
    min: Any = None,  # noqa: A002
    for_: timedelta | None = None,
    timeout: timedelta = _TIMEOUT,
    poll: timedelta = _POLL,
) -> Any:
    """One forwarding point for every named expect method below."""
    return await expect_metric(
        spec.bind(read, label=label),
        approx=approx,
        tol=tol,
        within=within,
        max=max,
        min=min,
        for_=for_,
        timeout=timeout,
        poll=poll,
    )


class ComponentExpect:
    """Settle-aware assertions on one component's telemetry."""

    def __init__(self, site: Site, component_id: int, mg_id: int | None) -> None:
        self._site = site
        self._id = component_id
        self._mg = mg_id

    async def active_power(
        self,
        *,
        approx: Power | None = None,
        tol: Power | None = None,
        within: tuple[Power, Power] | None = None,
        max: Power | None = None,
        min: Power | None = None,
        for_: timedelta | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Power | list[Power | None] | None:
        return await _expect(
            ACTIVE_POWER,
            lambda: self._site.active_power(self._id, self._mg),
            f"component {self._id} active_power",
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            for_=for_,
            timeout=timeout,
            poll=poll,
        )

    async def soc(
        self,
        *,
        approx: Percentage | None = None,
        tol: Percentage | None = None,
        within: tuple[Percentage, Percentage] | None = None,
        max: Percentage | None = None,
        min: Percentage | None = None,
        for_: timedelta | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Percentage | list[Percentage | None] | None:
        return await _expect(
            SOC,
            lambda: self._site.soc(self._id, self._mg),
            f"component {self._id} soc",
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            for_=for_,
            timeout=timeout,
            poll=poll,
        )


class ComponentHandle:
    """A component in a running site, acted on by intent."""

    def __init__(self, site: Site, component_id: int, mg_id: int | None = None) -> None:
        self._site = site
        self._id = component_id
        self._mg = mg_id

    def command(
        self,
        *,
        active_power: Power | None = None,
        bounds: tuple[Power, Power] | None = None,
        lifetime: timedelta | None = None,
    ) -> ComponentHandle:
        """Issue a control command as the app would — through the gRPC gateway.

        An ``active_power`` outside the live envelope raises ``SetpointRejected``.
        ``bounds`` augments (narrows) the effective active-power bounds.
        """
        if active_power is not None:
            self._site.set_active_power(
                self._id, active_power, lifetime=lifetime, mg_id=self._mg
            )
        if bounds is not None:
            self._site.augment_bounds(self._id, bounds[0], bounds[1], mg_id=self._mg)
        return self

    def status(
        self,
        *,
        health: Health | None = None,
        command_mode: CommandMode | None = None,
        telemetry_mode: TelemetryMode | None = None,
    ) -> ComponentHandle:
        """Set the component's operational status — its reported health and its
        command / telemetry channel modes (this is how you inject faults).

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
            self._site.control_component(self._id, "status", payload, self._mg)
        return self

    def drive(
        self,
        *,
        power: Power | RawLisp | None = None,
        sunlight: Percentage | None = None,
    ) -> ComponentHandle:
        """Drive the environment: a meter's published power, a PV's sunlight.

        Constant values go over the typed control API (rejections raise
        ``ControlRejected``); a ``RawLisp`` power (a lambda or symbol,
        re-resolved every tick) goes through the microgrid's eval.
        """
        payload: dict[str, float] = {}
        if isinstance(power, RawLisp):
            self._eval(f"(set-meter-power {self._id} {to_lisp_atom(power)})")
        elif power is not None:
            payload["power_w"] = power.as_watts()
        if sunlight is not None:
            payload["sunlight_pct"] = sunlight.as_percent()
        if payload:
            self._site.control_component(self._id, "drive", payload, self._mg)
        return self

    def active_power(self) -> Power | None:
        """A single sample of this component's active power (gRPC)."""
        return self._site.active_power(self._id, self._mg)

    def soc(self) -> Percentage | None:
        """A single sample of this battery's state of charge (gRPC)."""
        return self._site.soc(self._id, self._mg)

    @property
    def expect(self) -> ComponentExpect:
        """Settle-aware assertions on this component."""
        return ComponentExpect(self._site, self._id, self._mg)

    def _eval(self, expr: str) -> None:
        # Site.eval reports interpreter rejections as ok: False — surface
        # them, or a status()/drive() typo silently no-ops and the test
        # asserts against an unfaulted, undriven sim.
        result = self._site.eval(expr, self._site._resolve_mg(self._mg))
        if not result.get("ok", True):
            raise EvalRejected(f"eval of {expr!r} failed: {result.get('error')}")


class MicrogridExpect:
    """Settle-aware assertions on a microgrid's graph-derived aggregates."""

    def __init__(self, site: Site, mg_id: int | None) -> None:
        self._site = site
        self._mg = mg_id

    # --- power (instantaneous — settles) -----------------------------------

    async def grid_power(
        self,
        *,
        approx: Power | None = None,
        tol: Power | None = None,
        within: tuple[Power, Power] | None = None,
        max: Power | None = None,
        min: Power | None = None,
        for_: timedelta | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Power | list[Power | None] | None:
        return await _expect(
            GRID_POWER,
            lambda: self._site.grid_power(self._mg),
            None,
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            for_=for_,
            timeout=timeout,
            poll=poll,
        )

    async def pv_power(
        self,
        *,
        approx: Power | None = None,
        tol: Power | None = None,
        within: tuple[Power, Power] | None = None,
        max: Power | None = None,
        min: Power | None = None,
        for_: timedelta | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Power | list[Power | None] | None:
        return await _expect(
            PV_POWER,
            lambda: self._site.pv_power(self._mg),
            None,
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            for_=for_,
            timeout=timeout,
            poll=poll,
        )

    async def consumer_power(
        self,
        *,
        approx: Power | None = None,
        tol: Power | None = None,
        within: tuple[Power, Power] | None = None,
        max: Power | None = None,
        min: Power | None = None,
        for_: timedelta | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Power | list[Power | None] | None:
        return await _expect(
            CONSUMER_POWER,
            lambda: self._site.consumer_power(self._mg),
            None,
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            for_=for_,
            timeout=timeout,
            poll=poll,
        )

    async def battery_power(
        self,
        *,
        approx: Power | None = None,
        tol: Power | None = None,
        within: tuple[Power, Power] | None = None,
        max: Power | None = None,
        min: Power | None = None,
        for_: timedelta | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Power | list[Power | None] | None:
        return await _expect(
            BATTERY_POWER,
            lambda: self._site.battery_power(self._mg),
            None,
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            for_=for_,
            timeout=timeout,
            poll=poll,
        )

    # --- energy (cumulative — a one-shot check, not settling) --------------
    # No ``for_``: a running total is checked once, so a hold window does
    # not apply (the engine rejects it).

    async def grid_energy(
        self,
        *,
        approx: Energy | None = None,
        tol: Energy | None = None,
        within: tuple[Energy, Energy] | None = None,
        max: Energy | None = None,
        min: Energy | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Energy | list[Energy | None] | None:
        """Assert on cumulative net grid energy (import positive)."""
        return await _expect(
            GRID_ENERGY,
            lambda: self._site.grid_energy(self._mg),
            None,
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            timeout=timeout,
            poll=poll,
        )

    async def consumer_energy(
        self,
        *,
        approx: Energy | None = None,
        tol: Energy | None = None,
        within: tuple[Energy, Energy] | None = None,
        max: Energy | None = None,
        min: Energy | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Energy | list[Energy | None] | None:
        """Assert on cumulative consumer (load) energy."""
        return await _expect(
            CONSUMER_ENERGY,
            lambda: self._site.consumer_energy(self._mg),
            None,
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            timeout=timeout,
            poll=poll,
        )

    async def pv_energy(
        self,
        *,
        approx: Energy | None = None,
        tol: Energy | None = None,
        within: tuple[Energy, Energy] | None = None,
        max: Energy | None = None,
        min: Energy | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Energy | list[Energy | None] | None:
        """Assert on cumulative PV energy (production negative)."""
        return await _expect(
            PV_ENERGY,
            lambda: self._site.pv_energy(self._mg),
            None,
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            timeout=timeout,
            poll=poll,
        )

    async def battery_energy(
        self,
        *,
        approx: Energy | None = None,
        tol: Energy | None = None,
        within: tuple[Energy, Energy] | None = None,
        max: Energy | None = None,
        min: Energy | None = None,
        timeout: timedelta = _TIMEOUT,
        poll: timedelta = _POLL,
    ) -> Energy | list[Energy | None] | None:
        """Assert on cumulative net battery-pool energy (discharge negative)."""
        return await _expect(
            BATTERY_ENERGY,
            lambda: self._site.battery_energy(self._mg),
            None,
            approx=approx,
            tol=tol,
            within=within,
            max=max,
            min=min,
            timeout=timeout,
            poll=poll,
        )


class MicrogridHandle:
    """A specific microgrid within a running site (for multi-microgrid setups)."""

    def __init__(self, site: Site, mg_id: int) -> None:
        self._site = site
        self._mg = mg_id

    def component(self, target: Component | int) -> ComponentHandle:
        cid = self._site._component_id_of(target)
        return ComponentHandle(self._site, cid, self._mg)

    def __getitem__(self, target: Component | int) -> ComponentHandle:
        return self.component(target)

    @property
    def expect(self) -> MicrogridExpect:
        return MicrogridExpect(self._site, self._mg)

    def grid_power(self) -> Power | None:
        return self._site.grid_power(self._mg)

    def pv_power(self) -> Power | None:
        return self._site.pv_power(self._mg)

    def consumer_power(self) -> Power | None:
        return self._site.consumer_power(self._mg)

    def battery_power(self) -> Power | None:
        return self._site.battery_power(self._mg)
