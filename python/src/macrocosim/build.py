"""Pythonic topology builder that emits macrocosim's ``(make-*)`` Lisp.

The spec *is* the graph: nest children with ``successors=[…]``, exactly as
``(make-grid-connection-point … :successors (list (make-meter …)))`` in a
hand-written config. Each constructor returns a :class:`Component` node;
:meth:`Microgrid.to_lisp` walks the tree and renders the nested form 1:1.

Values are strongly typed — physical quantities are
`frequenz-quantities <https://pypi.org/project/frequenz-quantities/>`_
(``Power``, ``Energy``, ``Percentage``, …), times are :mod:`datetime`, and
runtime knobs are the :mod:`macrocosim.enums` enums. :func:`to_lisp_atom`
converts any of these to the Lisp-supported literal macrocosim reads;
:func:`raw` is the one escape hatch for splicing a literal form.
"""

from __future__ import annotations

import inspect
import os
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import time, timedelta
from enum import StrEnum
from functools import cache
from pathlib import Path
from typing import Any, NoReturn, Protocol, TypeAlias, TypeVar, runtime_checkable

from frequenz.quantities import (
    ApparentPower,
    Current,
    Energy,
    Percentage,
    Power,
    Quantity,
    ReactivePower,
    Voltage,
)

from .enums import CommandMode, EvIdle, EvPreset, Health, TelemetryMode
from .metrics import ACTIVE_POWER, REACTIVE_POWER, SOC, STORED_ENERGY
from .signals import DrivenSignal, Kind, SettingSignal, Signal, _require


@dataclass(frozen=True)
class RawLisp:
    """A literal Lisp form spliced verbatim into the emitted config."""

    text: str


def raw(text: str) -> RawLisp:
    """Splice a literal Lisp form (a value, or a whole ``(make-* …)``)."""
    return RawLisp(text)


# The types a power, reactive-power or sunlight source takes: a typed
# constant, or a raw Lisp form (a lambda or symbol) re-read every tick.
_POWER_SOURCE: Kind = (Power, RawLisp)
_REACTIVE_SOURCE: Kind = (ReactivePower, RawLisp)
_SUNLIGHT_SOURCE: Kind = (Percentage, RawLisp)

# A value that can appear in an emitted plist: a Lisp scalar, a typed physical
# quantity, a datetime, an enum symbol, or a spliced raw form.
Value: TypeAlias = (
    bool | int | float | str | Quantity | timedelta | time | StrEnum | RawLisp
)
# A microgrid's topology: one root, a raw form, a sequence of them, or none.
Topology: TypeAlias = "Component | RawLisp | Sequence[Component | RawLisp] | None"


@runtime_checkable
class LispRenderable(Protocol):
    """Anything that renders itself to a Lisp config form (a ``Microgrid``)."""

    def to_lisp(self) -> str: ...


# A single config to launch: a ``.lisp`` path or one renderable (a ``Microgrid``).
LaunchConfig: TypeAlias = "str | os.PathLike[str] | LispRenderable"
# A config to render: a ``LaunchConfig`` or a sequence of renderables (e.g. a
# ``Microgrid`` plus ``Scenario`` s), for the stepped headless runner.
ConfigSource: TypeAlias = "LaunchConfig | Sequence[LispRenderable]"


def _lisp_string(value: str) -> str:
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def to_lisp_atom(value: Value) -> str:
    """Render a typed Python value as the Lisp literal macrocosim reads.

    Physical quantities collapse to their SI base unit (``Power`` → watts,
    ``Energy`` → watt-hours, ``Percentage`` → percent, ``Frequency`` → hertz),
    a ``timedelta`` to seconds, a ``datetime.time`` to an ``"HH:MM:SS"`` string,
    an enum to its ``'symbol``, and ``bool`` to ``t`` / ``nil``.
    """
    if isinstance(value, RawLisp):
        return value.text
    if isinstance(value, bool):
        return "t" if value else "nil"
    if isinstance(value, Quantity):
        return repr(value.base_value)
    if isinstance(value, timedelta):
        return repr(value.total_seconds())
    if isinstance(value, time):
        return _lisp_string(value.isoformat())
    if isinstance(value, StrEnum):
        return f"'{value}"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    return _lisp_string(value)


# Each builder argument that carries a unit: the plist key it is written
# under and the type it must have.
_QUANTITY_ARGS: dict[str, tuple[str, Kind]] = {
    "rated_fuse_current": ("rated-fuse-current-a", Current),
    "stream_jitter": ("stream-jitter-pct", Percentage),
    "interval": ("interval-s", timedelta),
    "command_delay": ("command-delay-s", timedelta),
    "reactive_command_delay": ("reactive-command-delay-s", timedelta),
    "power": ("power-w", _POWER_SOURCE),
    "reactive_power": ("reactive-power-var", _REACTIVE_SOURCE),
    "reactive_apparent": ("reactive-apparent-va", ApparentPower),
    "capacity": ("capacity-wh", Energy),
    "initial_soc": ("initial-soc-pct", Percentage),
    "soc_lower": ("soc-lower-pct", Percentage),
    "soc_upper": ("soc-upper-pct", Percentage),
    "soc_protect_margin": ("soc-protect-margin-pct", Percentage),
    "voltage": ("voltage-v", Voltage),
    "sunlight": ("sunlight-pct", _SUNLIGHT_SOURCE),
}


# Builder arguments that were renamed, and the argument that replaced each.
_RETIRED_ARGS: dict[str, str] = {
    "stream_jitter_pct": "stream_jitter",
    "reactive_apparent_va": "reactive_apparent",
    "ramp_rate": "ramp_rate_w_per_s",
    "reactive_ramp_rate": "reactive_ramp_rate_var_per_s",
    "demand_kg_h": "demand_kg_per_s",
}

# Each quantity argument's plist key spelled as an argument name
# (``power_w``), and the typed argument that writes that key.
_UNIT_SPELLED_ARGS: dict[str, str] = {
    lisp_key.replace("-", "_"): arg for arg, (lisp_key, _) in _QUANTITY_ARGS.items()
} | {"rated_lower_w": "rated", "rated_upper_w": "rated"}


@cache
def _params(builder: Callable[..., object]) -> frozenset[str]:
    """A builder's named (keyword-only) parameters; ``**extra`` is not one."""
    return frozenset(
        name
        for name, param in inspect.signature(builder).parameters.items()
        if param.kind is inspect.Parameter.KEYWORD_ONLY
    )


def _refuse_renamed(key: str, params: frozenset[str]) -> NoReturn:
    """Raise :class:`TypeError` for a retired or unit-spelled argument.

    The message names the argument to use when the builder has it among
    its named parameters ``params``.
    """
    retired = key in _RETIRED_ARGS
    new = _RETIRED_ARGS[key] if retired else _UNIT_SPELLED_ARGS[key]
    if new not in params:
        raise TypeError(f"unexpected argument {key}")
    if retired:
        raise TypeError(f"{key} was renamed; use {new}")
    raise TypeError(f"no argument {key}; use {new}, a typed value")


def _normalize(
    kwargs: Mapping[str, Value | None], params: frozenset[str]
) -> dict[str, Value]:
    """Resolve a builder's constructor kwargs to a plist-key → value dict.

    ``kwargs`` holds every named argument of the builder (``None`` when
    not given) apart from ``rated``, and its ``**extra``; ``params`` is
    the builder's named parameters. Each quantity argument is
    type-checked and written under its unit suffixed key; value
    conversion is :func:`to_lisp_atom`'s job, at emit time, apart from
    a current (whole amperes). A retired argument name, or a plist
    key spelled as an argument (``power_w``), raises :class:`TypeError`.
    """
    out: dict[str, Value] = {}
    for key, value in kwargs.items():
        if key in _RETIRED_ARGS or key in _UNIT_SPELLED_ARGS:
            _refuse_renamed(key, params)
        if value is None:
            continue
        if key in _QUANTITY_ARGS:
            lisp_key, kind = _QUANTITY_ARGS[key]
            _require(value, kind, key)
            if isinstance(value, Current):
                value = _amps(value)
            out[lisp_key] = value
        else:
            out[key.replace("_", "-")] = value
    return out


@dataclass
class Component:
    """One node in the topology tree — a ``(make-*)`` form and its children.

    Before launch it is the spec; after ``macrocosim.aio.launch`` binds
    the topology, the same object is the *live handle*: its typed signal
    properties (``health`` here; ``power`` / ``soc`` / ... on the
    category subclasses) read, assert, and drive the running component.
    """

    make: str
    args: dict[str, Value] = field(default_factory=dict)
    successors: list[Component] = field(default_factory=list)
    # The running aio site this object is live on (None until launch
    # binds it). One site at a time: build a fresh topology per launch.
    _site: Any = field(default=None, repr=False, compare=False)
    # The owning microgrid's id (None = the site's default microgrid),
    # recorded at bind time so signals route to the right microgrid.
    _microgrid_id: Any = field(default=None, repr=False, compare=False)

    def to_lisp(self) -> str:
        parts = [f":{key} {to_lisp_atom(val)}" for key, val in self.args.items()]
        if self.successors:
            children = " ".join(c.to_lisp() for c in self.successors)
            parts.append(f":successors (list {children})")
        body = (" " + " ".join(parts)) if parts else ""
        return f"({self.make}{body})"

    # --- live-handle plumbing (bound by macrocosim.aio.launch) ------------

    @property
    def id(self) -> int:
        """The explicit ``id=`` this builder was given."""
        cid = self.args.get("id")
        if not isinstance(cid, int):
            # ValueError, like the sync client raises for the same
            # mistake — code catching one works on both flavors.
            raise ValueError(f"{self.make}: needs an explicit id= to be referenced")
        return cid

    def _bind(self, site: Any, microgrid_id: int | None = None) -> None:
        # No id check here: a component without an explicit id= simply
        # has no usable signals (the property raises on first use), but
        # its topology still launches.
        if self._site is not None:
            raise RuntimeError(
                f"{self.make} id={self.args.get('id')} is already bound to a "
                "running site; build a fresh topology per launch"
            )
        self._site = site
        self._microgrid_id = microgrid_id

    def _unbind(self) -> None:
        self._site = None
        self._microgrid_id = None

    def _live(self) -> Any:
        if self._site is None:
            raise RuntimeError(
                f"{self.make} id={self.args.get('id')} is not bound to a "
                "running site — launch its topology with macrocosim.aio.launch"
            )
        return self._site

    @property
    def health(self) -> SettingSignal[Health]:
        """The component's reported health — set it to inject faults."""

        async def set_(value: Health) -> None:
            await self._live().control_component(
                self.id, "status", {"health": value.value}, self._microgrid_id
            )

        return SettingSignal(
            set_,
            f"{self.make} {self.args.get('id')} health",
            kind=Health,
            cue=lambda v: f"(set-component-health {self.id} {to_lisp_atom(v)})",
        )


class Meter(Component):
    """A meter: its published power is both measured and drivable."""

    @property
    def power(self) -> DrivenSignal[Power]:
        """The meter's active power — read/expect the telemetry, set the load."""

        async def read() -> Power | None:
            return await self._live().active_power(self.id, self._microgrid_id)

        async def set_(value: Power) -> None:
            await self._live().control_component(
                self.id, "drive", {"power_w": value.as_watts()}, self._microgrid_id
            )

        return DrivenSignal(
            ACTIVE_POWER,
            read,
            set_,
            f"meter {self.args.get('id')} power",
            kind=Power,
            cue_kind=_POWER_SOURCE,
            check_ref=lambda: (self.id, "active-power"),
            cue=lambda v: f"(set-meter-power {self.id} {to_lisp_atom(v)})",
        )

    @property
    def reactive_power(self) -> DrivenSignal[ReactivePower]:
        """The meter's reactive power — read the telemetry, set the VAr load."""

        async def read() -> ReactivePower | None:
            return await self._live().reactive_power(self.id, self._microgrid_id)

        async def set_(value: ReactivePower) -> None:
            await self._live().control_component(
                self.id,
                "drive",
                {"reactive_power_var": value.as_volt_amperes_reactive()},
                self._microgrid_id,
            )

        return DrivenSignal(
            REACTIVE_POWER,
            read,
            set_,
            f"meter {self.args.get('id')} reactive_power",
            kind=ReactivePower,
            cue_kind=_REACTIVE_SOURCE,
            check_ref=lambda: (self.id, "reactive-power"),
            cue=lambda v: f"(set-meter-reactive-power {self.id} {to_lisp_atom(v)})",
        )


class Battery(Component):
    """A battery: charge state is both measurable and arrangeable."""

    @property
    def soc(self) -> DrivenSignal[Percentage]:
        """State of charge — read/expect it, or teleport it to arrange a test."""

        async def read() -> Percentage | None:
            return await self._live().soc(self.id, self._microgrid_id)

        async def set_(value: Percentage) -> None:
            await self._live().control_component(
                self.id, "drive", {"soc_pct": value.as_percent()}, self._microgrid_id
            )

        return DrivenSignal(
            SOC,
            read,
            set_,
            f"battery {self.args.get('id')} soc",
            kind=Percentage,
            check_ref=lambda: (self.id, "soc"),
            cue=lambda v: f"(set-battery-soc {self.id} {to_lisp_atom(v)})",
        )

    @property
    def stored_energy(self) -> Signal[Energy]:
        """Energy held right now (SoC × capacity) — component *state*.

        Not to be confused with the site's ``battery_energy`` aggregate,
        which is the cumulative *flow* through the pool (the integral of
        ``battery_power``).
        """
        capacity = self.args.get("capacity-wh")
        if not isinstance(capacity, Energy):
            raise RuntimeError(
                f"battery {self.args.get('id')}: stored_energy needs "
                "capacity= on the builder"
            )

        async def read() -> Energy | None:
            soc = await self._live().soc(self.id, self._microgrid_id)
            if soc is None:
                return None
            return capacity * (soc.as_percent() / 100.0)

        return Signal(STORED_ENERGY, read, f"battery {self.args.get('id')} stored_energy")


class BatteryInverter(Component):
    """A battery inverter: power is measured; setting it is ``command()``'s
    job (the production gateway path), never a test-side ``set``."""

    @property
    def power(self) -> Signal[Power]:
        """The inverter's active power (read/expect only)."""

        async def read() -> Power | None:
            return await self._live().active_power(self.id, self._microgrid_id)

        return Signal(
            ACTIVE_POWER,
            read,
            f"battery_inverter {self.args.get('id')} power",
            check_ref=lambda: (self.id, "active-power"),
        )


class SolarInverter(Component):
    """A PV inverter: power is measured, sunlight is the drivable input."""

    @property
    def power(self) -> Signal[Power]:
        """The inverter's active power (read/expect only)."""

        async def read() -> Power | None:
            return await self._live().active_power(self.id, self._microgrid_id)

        return Signal(
            ACTIVE_POWER,
            read,
            f"solar_inverter {self.args.get('id')} power",
            check_ref=lambda: (self.id, "active-power"),
        )

    @property
    def sunlight(self) -> SettingSignal[Percentage]:
        """The irradiance driving the PV model (write-only)."""

        async def set_(value: Percentage) -> None:
            await self._live().control_component(
                self.id,
                "drive",
                {"sunlight_pct": value.as_percent()},
                self._microgrid_id,
            )

        return SettingSignal(
            set_,
            f"solar_inverter {self.args.get('id')} sunlight",
            kind=Percentage,
            cue_kind=_SUNLIGHT_SOURCE,
            cue=lambda v: f"(set-solar-sunlight {self.id} {to_lisp_atom(v)})",
        )


_C = TypeVar("_C", bound=Component)


def _component(
    make: str,
    args: Mapping[str, Value | None],
    *,
    params: frozenset[str],
    rated: tuple[Power, Power] | None = None,
    successors: Sequence[Component] | None = None,
    cls: type[_C],
) -> _C:
    """Build a ``make`` node from a builder's ``args`` and ``rated``.

    ``params`` is the builder's named parameters, from :func:`_params`.
    """
    normalized = _normalize(args, params)
    if rated is not None:
        lower, upper = rated
        _require(lower, Power, "rated")
        _require(upper, Power, "rated")
        normalized["rated-lower-w"], normalized["rated-upper-w"] = lower, upper
    return cls(make, normalized, list(successors or []))


def _amps(value: Current) -> int:
    """A current as whole amperes (the server's fuse-rating unit)."""
    return int(round(value.base_value))


# --- constructors (parents take successors; leaves don't) -----------------


def grid(
    *,
    id: int | None = None,
    name: str | None = None,
    rated: tuple[Power, Power] | None = None,
    rated_fuse_current: Current | None = None,
    stream_jitter: Percentage | None = None,
    health: Health | None = None,
    telemetry_mode: TelemetryMode | None = None,
    command_mode: CommandMode | None = None,
    successors: Sequence[Component] | None = None,
    **extra: Value,
) -> Component:
    """A grid connection point (the microgrid's boundary to the public grid)."""
    args = {
        "id": id,
        "name": name,
        "rated_fuse_current": rated_fuse_current,
        "stream_jitter": stream_jitter,
        "health": health,
        "telemetry_mode": telemetry_mode,
        "command_mode": command_mode,
        **extra,
    }
    return _component(
        "make-grid-connection-point",
        args,
        params=_params(grid),
        rated=rated,
        successors=successors,
        cls=Component,
    )


def meter(
    *,
    id: int | None = None,
    name: str | None = None,
    power: Power | RawLisp | None = None,
    reactive_power: ReactivePower | RawLisp | None = None,
    power_factor: float | None = None,
    leading: bool | None = None,
    interval: timedelta | None = None,
    hidden: bool | None = None,
    stream_jitter: Percentage | None = None,
    health: Health | None = None,
    telemetry_mode: TelemetryMode | None = None,
    command_mode: CommandMode | None = None,
    successors: Sequence[Component] | None = None,
    **extra: Value,
) -> Meter:
    """A meter. ``power`` seeds its published load (a ``Power``, or ``raw(...)``).

    ``reactive_power`` seeds a constant VAr load (a ``ReactivePower``, or
    ``raw(...)``); ``power_factor`` instead derives it from the meter's own
    active power, held at that ratio (0.0, 1.0]. ``reactive_power`` and
    ``power_factor`` are mutually exclusive — the server rejects both set.
    ``leading`` makes a ``power_factor`` load capacitive instead of the
    default inductive; it is meaningless without ``power_factor``.

    ``interval`` is the telemetry stream period; ``hidden`` keeps the
    meter out of the visible topology (helper meters).
    """
    args = {
        "id": id,
        "name": name,
        "power": power,
        "reactive_power": reactive_power,
        "power_factor": power_factor,
        "leading": leading,
        "interval": interval,
        "hidden": hidden,
        "stream_jitter": stream_jitter,
        "health": health,
        "telemetry_mode": telemetry_mode,
        "command_mode": command_mode,
        **extra,
    }
    return _component(
        "make-meter", args, params=_params(meter), successors=successors, cls=Meter
    )


def battery_inverter(
    *,
    id: int | None = None,
    name: str | None = None,
    rated: tuple[Power, Power] | None = None,
    interval: timedelta | None = None,
    command_delay: timedelta | None = None,
    ramp_rate_w_per_s: float | None = None,
    reactive_pf_limit: float | None = None,
    reactive_apparent: ApparentPower | None = None,
    reactive_command_delay: timedelta | None = None,
    reactive_ramp_rate_var_per_s: float | None = None,
    stream_jitter: Percentage | None = None,
    health: Health | None = None,
    telemetry_mode: TelemetryMode | None = None,
    command_mode: CommandMode | None = None,
    successors: Sequence[Component] | None = None,
    **extra: Value,
) -> BatteryInverter:
    """A battery inverter; give it a ``battery`` child via ``successors``.

    ``command_delay`` models the device latency before a setpoint starts
    tracking; ``ramp_rate_w_per_s`` is the slew in W/s. The ``reactive_*`` knobs
    cap and pace reactive power: ``reactive_pf_limit`` (|Q| ≤ k·|P|,
    0 disables), ``reactive_apparent`` (P² + Q² ≤ apparent², 0
    disables), and ``reactive_ramp_rate_var_per_s`` in VAr/s.
    """
    args = {
        "id": id,
        "name": name,
        "interval": interval,
        "command_delay": command_delay,
        "ramp_rate_w_per_s": ramp_rate_w_per_s,
        "reactive_pf_limit": reactive_pf_limit,
        "reactive_apparent": reactive_apparent,
        "reactive_command_delay": reactive_command_delay,
        "reactive_ramp_rate_var_per_s": reactive_ramp_rate_var_per_s,
        "stream_jitter": stream_jitter,
        "health": health,
        "telemetry_mode": telemetry_mode,
        "command_mode": command_mode,
        **extra,
    }
    return _component(
        "make-battery-inverter",
        args,
        params=_params(battery_inverter),
        rated=rated,
        successors=successors,
        cls=BatteryInverter,
    )


def solar_inverter(
    *,
    id: int | None = None,
    name: str | None = None,
    rated: tuple[Power, Power] | None = None,
    sunlight: Percentage | RawLisp | None = None,
    interval: timedelta | None = None,
    command_delay: timedelta | None = None,
    ramp_rate_w_per_s: float | None = None,
    reactive_pf_limit: float | None = None,
    reactive_apparent: ApparentPower | None = None,
    reactive_command_delay: timedelta | None = None,
    reactive_ramp_rate_var_per_s: float | None = None,
    stream_jitter: Percentage | None = None,
    health: Health | None = None,
    telemetry_mode: TelemetryMode | None = None,
    command_mode: CommandMode | None = None,
    successors: Sequence[Component] | None = None,
    **extra: Value,
) -> SolarInverter:
    """A solar (PV) inverter. ``sunlight`` seeds its irradiance (a ``Percentage``,
    or ``raw(...)``).

    Latency, slew, and reactive knobs as on ``battery_inverter``.
    """
    args = {
        "id": id,
        "name": name,
        "sunlight": sunlight,
        "interval": interval,
        "command_delay": command_delay,
        "ramp_rate_w_per_s": ramp_rate_w_per_s,
        "reactive_pf_limit": reactive_pf_limit,
        "reactive_apparent": reactive_apparent,
        "reactive_command_delay": reactive_command_delay,
        "reactive_ramp_rate_var_per_s": reactive_ramp_rate_var_per_s,
        "stream_jitter": stream_jitter,
        "health": health,
        "telemetry_mode": telemetry_mode,
        "command_mode": command_mode,
        **extra,
    }
    return _component(
        "make-solar-inverter",
        args,
        params=_params(solar_inverter),
        rated=rated,
        successors=successors,
        cls=SolarInverter,
    )


def battery(
    *,
    id: int | None = None,
    name: str | None = None,
    capacity: Energy | None = None,
    initial_soc: Percentage | None = None,
    rated: tuple[Power, Power] | None = None,
    soc_lower: Percentage | None = None,
    soc_upper: Percentage | None = None,
    soc_protect_margin: Percentage | None = None,
    voltage: Voltage | None = None,
    interval: timedelta | None = None,
    stream_jitter: Percentage | None = None,
    health: Health | None = None,
    telemetry_mode: TelemetryMode | None = None,
    command_mode: CommandMode | None = None,
    **extra: Value,
) -> Battery:
    """A battery (leaf). ``capacity`` is an ``Energy``.

    ``initial_soc`` seeds the charge state; physics evolves it from the
    first tick (read or teleport it later via the ``soc`` signal).
    ``rated`` bounds the DC power envelope (lower, upper);
    ``soc_lower`` / ``soc_upper`` set the operating band, and the
    protective taper starts ``soc_protect_margin`` inside it.
    """
    args = {
        "id": id,
        "name": name,
        "capacity": capacity,
        "initial_soc": initial_soc,
        "soc_lower": soc_lower,
        "soc_upper": soc_upper,
        "soc_protect_margin": soc_protect_margin,
        "voltage": voltage,
        "interval": interval,
        "stream_jitter": stream_jitter,
        "health": health,
        "telemetry_mode": telemetry_mode,
        "command_mode": command_mode,
        **extra,
    }
    return _component(
        "make-battery", args, params=_params(battery), rated=rated, cls=Battery
    )


def ev_charger(
    *,
    id: int | None = None,
    name: str | None = None,
    rated: tuple[Power, Power] | None = None,
    phases: int | None = None,
    idle: EvIdle | None = None,
    interval: timedelta | None = None,
    command_delay: timedelta | None = None,
    ramp_rate_w_per_s: float | None = None,
    stream_jitter: Percentage | None = None,
    resume_on_recovery: bool | None = None,
    health: Health | None = None,
    telemetry_mode: TelemetryMode | None = None,
    command_mode: CommandMode | None = None,
    **extra: Value,
) -> Component:
    """An EV charger (leaf). Starts empty; ``Site.plug_ev`` plugs a car
    into it, which is runtime state and not part of the build.

    ``phases`` is 1 or 3. ``idle`` says what the charger offers with no
    command standing: ``EvIdle.PAUSED`` (nothing, the default) or
    ``EvIdle.FULL`` (its whole rating). A charger whose health goes to
    ``error`` or ``standby`` trips to zero and waits for a new setpoint;
    ``resume_on_recovery=True`` keeps the armed command through the fault.

    The charger has no pack of its own, so the pack kwargs it used to
    take raise :class:`TypeError` here rather than reaching the server
    (which accepts and ignores them, for the sake of files an older
    binary wrote). Give the car its pack instead, with
    ``Site.plug_ev(..., capacity=...)``.
    """
    for retired in (
        "capacity",
        "initial_soc",
        "soc_lower",
        "soc_upper",
        "soc_protect_margin",
    ):
        if retired in extra:
            raise TypeError(
                f"ev_charger no longer takes {retired} — the pack belongs to"
                " the car; use Site.plug_ev"
            )
    args = {
        "id": id,
        "name": name,
        "phases": phases,
        "idle": idle,
        "interval": interval,
        "command_delay": command_delay,
        "ramp_rate_w_per_s": ramp_rate_w_per_s,
        "stream_jitter": stream_jitter,
        "resume_on_recovery": resume_on_recovery,
        "health": health,
        "telemetry_mode": telemetry_mode,
        "command_mode": command_mode,
        **extra,
    }
    return _component(
        "make-ev-charger", args, params=_params(ev_charger), rated=rated, cls=Component
    )


def plug_ev_form(
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
) -> str:
    """Render the ``(plug-ev …)`` form that plugs a preset car into a charger.

    The one spelling of the form: both the sync and the async ``Site.plug_ev``
    emit through it, so the two clients can never drift apart. ``preset`` may
    be a plain string, which is validated against :class:`EvPreset` here — an
    unknown name raises :class:`ValueError` before any eval leaves the client.
    ``capacity`` is the car's pack. ``taper_start`` is the SoC the
    constant-current phase ends at and ``taper_floor`` the share of full
    current the car still draws at 100 % SoC — the whole of the charging
    curve's shape. A bare number where a quantity belongs raises
    :class:`TypeError`.
    """
    # (argument, plist key, type, value); `to_lisp_atom` writes each
    # quantity in its base unit, the unit the key names.
    overrides: tuple[tuple[str, str, type, Value | None], ...] = (
        ("soc", "soc-pct", Percentage, soc),
        ("target_soc", "target-soc-pct", Percentage, target_soc),
        ("phases", "phases", object, phases),
        ("max_current", "max-current-a", Current, max_current),
        ("capacity", "capacity-wh", Energy, capacity),
        ("taper_start", "taper-start-pct", Percentage, taper_start),
        ("taper_floor", "taper-floor-pct", Percentage, taper_floor),
    )
    for name, _, kind, value in overrides:
        if value is not None:
            _require(value, kind, name)
    parts = [f"(plug-ev {component_id} {to_lisp_atom(EvPreset(preset))}"]
    for _, key, _, value in overrides:
        if value is not None:
            parts.append(f":{key} {to_lisp_atom(value)}")
    return " ".join(parts) + ")"


def drive_parts(
    component_id: int,
    power: Power | RawLisp | None,
    sunlight: Percentage | None,
) -> tuple[str | None, dict[str, float]]:
    """Split a ``drive`` call into its two halves, for the sync and the async
    client alike.

    Returns the ``(set-meter-power …)`` form a ``RawLisp`` power evals (or
    ``None``), and the control-API payload for the constant values. A bare
    number raises :class:`TypeError`.
    """
    if power is not None:
        _require(power, _POWER_SOURCE, "power")
    if sunlight is not None:
        _require(sunlight, Percentage, "sunlight")
    form: str | None = None
    payload: dict[str, float] = {}
    if isinstance(power, RawLisp):
        form = f"(set-meter-power {component_id} {to_lisp_atom(power)})"
    elif power is not None:
        payload["power_w"] = power.as_watts()
    if sunlight is not None:
        payload["sunlight_pct"] = sunlight.as_percent()
    return form, payload


def chp(
    *,
    id: int | None = None,
    name: str | None = None,
    stream_jitter: Percentage | None = None,
    health: Health | None = None,
    telemetry_mode: TelemetryMode | None = None,
    command_mode: CommandMode | None = None,
    **extra: Value,
) -> Component:
    """A combined heat-and-power unit (leaf).

    ``make-chp`` takes no rated bounds (unlike the other generators);
    anything else it grows lands here via ``extra``.
    """
    args = {
        "id": id,
        "name": name,
        "stream_jitter": stream_jitter,
        "health": health,
        "telemetry_mode": telemetry_mode,
        "command_mode": command_mode,
        **extra,
    }
    return _component("make-chp", args, params=_params(chp), cls=Component)


def steam_boiler(
    *,
    id: int | None = None,
    name: str | None = None,
    rated: tuple[Power, Power] | None = None,
    target_bar: float | None = None,
    max_bar: float | None = None,
    initial_bar: float | None = None,
    capacity_wh_per_bar: float | None = None,
    wh_per_kg: float | None = None,
    demand_kg_per_s: float | None = None,
    interval: timedelta | None = None,
    command_delay: timedelta | None = None,
    ramp_rate_w_per_s: float | None = None,
    stream_jitter: Percentage | None = None,
    health: Health | None = None,
    telemetry_mode: TelemetryMode | None = None,
    command_mode: CommandMode | None = None,
    **extra: Value,
) -> Component:
    """A steam boiler (leaf). Pressure-driven: it holds a steam pressure in
    bar between ``0`` and ``max_bar``, tracks toward ``target_bar``, and
    ``demand_kg_per_s`` of steam draw converts to heat loss via ``wh_per_kg``.

    ``capacity_wh_per_bar`` is the thermal mass expressed as watt-hours per
    bar of pressure change; ``initial_bar`` seeds the starting pressure.
    ``interval`` is the telemetry stream period; ``command_delay`` and
    ``ramp_rate_w_per_s`` (W/s) pace a setpoint as on ``battery_inverter``.
    """
    args = {
        "id": id,
        "name": name,
        "target_bar": target_bar,
        "max_bar": max_bar,
        "initial_bar": initial_bar,
        "capacity_wh_per_bar": capacity_wh_per_bar,
        "wh_per_kg": wh_per_kg,
        "demand_kg_per_s": demand_kg_per_s,
        "interval": interval,
        "command_delay": command_delay,
        "ramp_rate_w_per_s": ramp_rate_w_per_s,
        "stream_jitter": stream_jitter,
        "health": health,
        "telemetry_mode": telemetry_mode,
        "command_mode": command_mode,
        **extra,
    }
    return _component(
        "make-steam-boiler",
        args,
        params=_params(steam_boiler),
        rated=rated,
        cls=Component,
    )


def _emit_topology(topology: Topology) -> str:
    if topology is None:
        return "nil"
    if isinstance(topology, RawLisp):
        return topology.text
    if isinstance(topology, Component):
        return topology.to_lisp()
    # a sequence of roots / raw forms
    return " ".join(_emit_topology(t) for t in topology)


@dataclass
class Microgrid:
    """A microgrid topology that renders to a ``(make-microgrid …)`` form."""

    id: int
    name: str | None = None
    grpc_port: int | None = None
    tso: str | None = None
    topology: Topology = None

    def to_lisp(self) -> str:
        head = f":id {self.id}"
        if self.name is not None:
            head += f" :name {_lisp_string(self.name)}"
        if self.grpc_port is not None:
            head += f" :grpc-port {self.grpc_port}"
        if self.tso is not None:
            head += f" :tso {_lisp_string(self.tso)}"
        body = _emit_topology(self.topology)
        return f"(make-microgrid {head}\n :topology (lambda () {body}))\n"

    @classmethod
    def from_lisp_file(cls, path: str | Path) -> Path:
        """Use an existing ``.lisp`` config as-is (the escape hatch).

        Returns the path (not a ``Microgrid``) so ``launch()`` loads the file
        directly, preserving its directory for any relative ``(load …)``.
        """
        return Path(path)
