"""Process lifecycle and the ``Site`` handle onto a running macrocosim.

``launch()`` boots the ``macrocosim`` binary on ephemeral ports, waits for
the endpoint-emission handshake, and hands back a ``Site`` (a context
manager); ``connect()`` attaches to an already-running instance. ``Site``
is the handle through which tests read (component telemetry over gRPC,
microgrid aggregates over HTTP), mutate and assert via component handles, and
run scenarios. Reads return typed ``frequenz-quantities`` (``Power`` /
``Percentage``); writes take them too.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from datetime import timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any, TypeVar

from frequenz.quantities import Energy, Percentage, Power, ReactivePower

from ._http import EvalResult, HttpClient, control_path
from ._process import spawn_macrocosim, terminate, which_binary
from .build import LaunchConfig, plug_ev_form
from .enums import EvPreset
from .errors import EvalRejected

__all__ = [
    "MicrogridEndpoint",
    "Site",
    "connect",
    "launch",
    "which_binary",
]

if TYPE_CHECKING:
    from ._grpc import ComponentInfo, GrpcClient
    from .build import Component
    from .handles import ComponentHandle, MicrogridExpect, MicrogridHandle
    from .scenarios import Scenario, ScenarioRun

T = TypeVar("T")


@dataclass(frozen=True)
class MicrogridEndpoint:
    """One microgrid's resolved gRPC address."""

    id: int
    name: str
    grpc: str  # "host:port" of the Microgrid gRPC API


class Site:
    """A running macrocosim instance and its resolved endpoints.

    Use as a context manager; exiting tears the process down. Reads exposed
    here are the graph-derived formula aggregates (grid / consumer / PV /
    battery power) served over HTTP.
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
        self._http = HttpClient(f"http://{ui}")
        self._grpc_clients: dict[int, GrpcClient] = {}
        # Assertion reads run on asyncio.to_thread worker threads, so two
        # can race to build the same client; the lock makes it one-shot.
        self._grpc_lock = threading.Lock()

    @property
    def grpc(self) -> str:
        """``host:port`` of the first (default) microgrid's gRPC API."""
        return self.microgrids[self._resolve_mg(None)].grpc

    @property
    def grpc_url(self) -> str:
        """The default microgrid's gRPC API as a ``grpc://host:port`` URL.

        The scheme-prefixed form a gRPC client connects with — e.g.
        ``microgrid.initialize(server_url=site.grpc_url)``.
        """
        return f"grpc://{self.grpc}"

    def microgrid_grpc_url(self, mg_id: int) -> str:
        """A specific microgrid's gRPC API as a ``grpc://host:port`` URL."""
        return f"grpc://{self.microgrids[mg_id].grpc}"

    def _resolve_mg(self, mg_id: int | None) -> int:
        """The microgrid a per-microgrid call acts on: ``mg_id``,
        else the lowest id in :attr:`microgrids`."""
        if mg_id is not None:
            return mg_id
        if not self.microgrids:
            raise RuntimeError(
                "this Site has no microgrid endpoints; launch() discovers them, "
                "connect() needs microgrids={id: MicrogridEndpoint(...)}"
            )
        return min(self.microgrids)

    def grpc_client(self, mg_id: int | None = None) -> GrpcClient:
        """The gRPC client for a microgrid, connected on first use.

        Requires the ``grpc`` extra (``frequenz-client-microgrid``). One
        cached connection per microgrid — the same client the app under
        test would open against macrocosim.
        """
        mg = self._resolve_mg(mg_id)
        with self._grpc_lock:
            client = self._grpc_clients.get(mg)
            if client is None:
                from ._grpc import GrpcClient

                client = GrpcClient(self.microgrid_grpc_url(mg))
                self._grpc_clients[mg] = client
        return client

    # --- reads: component-level (gRPC — what the app under test sees) ------

    def components(self, mg_id: int | None = None) -> list[ComponentInfo]:
        """List the microgrid's components (id, category, name)."""
        return self.grpc_client(mg_id).components()

    def active_power(self, component_id: int, mg_id: int | None = None) -> Power | None:
        """A component's active power — one sample off its gRPC stream."""
        watts = self.grpc_client(mg_id).active_power(component_id)
        return None if watts is None else Power.from_watts(watts)

    def reactive_power(
        self, component_id: int, mg_id: int | None = None
    ) -> ReactivePower | None:
        """A component's reactive power — one sample off its gRPC stream."""
        vars_ = self.grpc_client(mg_id).reactive_power(component_id)
        return None if vars_ is None else ReactivePower.from_volt_amperes_reactive(vars_)

    def soc(self, component_id: int, mg_id: int | None = None) -> Percentage | None:
        """A battery's state of charge — one sample off its gRPC stream."""
        pct = self.grpc_client(mg_id).soc(component_id)
        return None if pct is None else Percentage.from_percent(pct)

    # --- fluent runtime mutation ------------------------------------------

    def _component_id_of(self, target: Component | int) -> int:
        """Resolve a builder Component (with explicit id) or int to an id."""
        from .build import Component

        if isinstance(target, Component):
            cid = target.args.get("id")
            if not isinstance(cid, int):
                raise ValueError(
                    "this call needs a component with an explicit id= to reference it"
                )
            return cid
        return int(target)

    def component(
        self, target: Component | int, mg_id: int | None = None
    ) -> ComponentHandle:
        """A handle onto one component (``site[id]`` is the same): ``command`` /
        ``status`` / ``drive``, reads (``active_power`` / ``soc``), and
        ``expect`` (assertions)."""
        from .handles import ComponentHandle

        return ComponentHandle(self, self._component_id_of(target), mg_id)

    def __getitem__(self, target: Component | int) -> ComponentHandle:
        return self.component(target)

    @property
    def expect(self) -> MicrogridExpect:
        """Settle-aware assertions on the default microgrid's graph aggregates
        (``grid_power`` / ``pv_power`` / ``consumer_power`` / ``battery_power``)."""
        from .handles import MicrogridExpect

        return MicrogridExpect(self, None)

    def microgrid(self, mg_id: int) -> MicrogridHandle:
        """A handle onto one microgrid (for multi-microgrid setups): ``at`` its
        components, ``expect`` its aggregates, read its formulas."""
        from .handles import MicrogridHandle

        return MicrogridHandle(self, mg_id)

    # --- writes: setpoints + bounds (gRPC — exercises the real gateway) ---

    def set_active_power(
        self,
        component_id: int,
        power: Power,
        *,
        lifetime: timedelta | None = None,
        mg_id: int | None = None,
    ) -> None:
        """Command a component's active-power setpoint; errors if the value is
        outside the live envelope, exactly as production does."""
        lifetime_s = lifetime.total_seconds() if lifetime is not None else None
        self.grpc_client(mg_id).set_active_power(
            component_id, power.as_watts(), lifetime_s=lifetime_s
        )

    def augment_bounds(
        self,
        component_id: int,
        lower: Power,
        upper: Power,
        mg_id: int | None = None,
    ) -> None:
        """Narrow a component's effective active-power bounds (TTL-limited)."""
        self.grpc_client(mg_id).augment_active_power_bounds(
            component_id, lower.as_watts(), upper.as_watts()
        )

    # --- reads: microgrid-level formula aggregates (HTTP) -----------------

    def latest(self, mg_id: int | None = None) -> dict[str, Any]:
        """Latest sample per formula/component stream, keyed by name."""
        mg = self._resolve_mg(mg_id)
        return self._http.get_json(f"/api/mg/{mg}/metrics/latest")

    def formula(self, name: str, mg_id: int | None = None) -> float | None:
        """Raw value of one formula stream (e.g. ``"grid_power"``), or None."""
        snap = self.latest(mg_id).get(name)
        return None if snap is None else snap.get("value")

    def _power_formula(self, name: str, mg_id: int | None) -> Power | None:
        value = self.formula(name, mg_id)
        return None if value is None else Power.from_watts(value)

    def grid_power(self, mg_id: int | None = None) -> Power | None:
        return self._power_formula("grid_power", mg_id)

    def pv_power(self, mg_id: int | None = None) -> Power | None:
        return self._power_formula("pv_power", mg_id)

    def consumer_power(self, mg_id: int | None = None) -> Power | None:
        return self._power_formula("consumer_power", mg_id)

    def battery_power(self, mg_id: int | None = None) -> Power | None:
        return self._power_formula("battery_pool_power", mg_id)

    # --- reads: cumulative energy (integrated server-side from the power
    # aggregates; signed like the power, so net across the bus) ------------

    def _energy_formula(self, name: str, mg_id: int | None) -> Energy | None:
        value = self.formula(name, mg_id)
        return None if value is None else Energy.from_watt_hours(value)

    def grid_energy(self, mg_id: int | None = None) -> Energy | None:
        """Cumulative net grid energy for the current run (import positive).

        A config hot-reload resets the site and starts a new run, so the
        total restarts at zero there — not only at launch.
        """
        return self._energy_formula("grid_energy", mg_id)

    def consumer_energy(self, mg_id: int | None = None) -> Energy | None:
        """Cumulative consumer (load) energy for the current run."""
        return self._energy_formula("consumer_energy", mg_id)

    def pv_energy(self, mg_id: int | None = None) -> Energy | None:
        """Cumulative PV energy for the current run (production negative)."""
        return self._energy_formula("pv_energy", mg_id)

    def battery_energy(self, mg_id: int | None = None) -> Energy | None:
        """Cumulative net battery-pool energy for the current run (discharge
        negative)."""
        return self._energy_formula("battery_pool_energy", mg_id)

    def eval(self, expr: str, mg_id: int | None = None) -> EvalResult:
        """Evaluate a raw Lisp form on the running interpreter."""
        return self._http.eval(expr, mg_id)

    def _eval_ok(self, expr: str, mg_id: int | None = None) -> EvalResult:
        """Eval and raise :class:`EvalRejected` on an interpreter rejection.

        The one choke point for programmatic eval — rejections can never
        silently no-op.
        """
        result = self.eval(expr, mg_id)
        if not result.get("ok", True):
            raise EvalRejected(f"eval of {expr!r} failed: {result.get('error')}")
        return result

    # --- EV chargers: plug state (Lisp-backed, read over HTTP) -----------

    def plug_ev(
        self,
        component_id: int,
        preset: EvPreset | str,
        *,
        soc: float | None = None,
        target_soc: float | None = None,
        phases: int | None = None,
        max_current_a: float | None = None,
        capacity: Energy | None = None,
        taper_start: float | None = None,
        taper_floor: float | None = None,
        mg_id: int | None = None,
    ) -> None:
        """Plug a preset car into charger ``component_id``."""
        self._eval_ok(
            plug_ev_form(
                component_id,
                preset,
                soc=soc,
                target_soc=target_soc,
                phases=phases,
                max_current_a=max_current_a,
                capacity=capacity,
                taper_start=taper_start,
                taper_floor=taper_floor,
            ),
            self._resolve_mg(mg_id),
        )

    def unplug_ev(self, component_id: int, mg_id: int | None = None) -> bool:
        """Unplug the car; False when the charger was already empty."""
        result = self._eval_ok(f"(unplug-ev {component_id})", self._resolve_mg(mg_id))
        return result.get("value") == "t"

    def ev_info(self, component_id: int, mg_id: int | None = None) -> dict[str, Any]:
        """The car plugged into ``component_id``: ``{"plugged": False}`` or the
        fields, plus ``presets`` (the catalog) either way."""
        mg = self._resolve_mg(mg_id)
        return self._http.get_json(f"/api/mg/{mg}/component/{component_id}/ev")

    def control_component(
        self,
        component_id: int,
        action: str,
        payload: dict[str, Any],
        mg_id: int | None = None,
    ) -> None:
        """POST a typed control request (``status`` / ``drive``) for a component.

        Rejections (unknown id, bad value) raise ``ControlRejected``.
        """
        path = control_path(component_id, action, self._resolve_mg(mg_id))
        self._http.control(path, payload)

    def scenario(self, name: str) -> ScenarioRun:
        """Handle onto a registered ``(define-scenario …)`` for run/report."""
        from .scenarios import ScenarioRun

        return ScenarioRun(self, name)

    def define_scenario(self, scenario: Scenario) -> ScenarioRun:
        """Register a Python-authored ``Scenario`` and return its ScenarioRun."""
        from .scenarios import ScenarioRun

        result = self.eval(scenario.to_lisp())
        if not result.get("ok", True):
            raise EvalRejected(f"define-scenario failed: {result.get('error')}")
        return ScenarioRun(self, scenario.name)

    def read_until(
        self,
        read: Callable[[], T],
        predicate: Callable[[T], bool],
        *,
        timeout: timedelta = timedelta(seconds=10),
        poll: timedelta = timedelta(milliseconds=250),
    ) -> T:
        """Poll ``read()`` until ``predicate`` holds or ``timeout`` elapses.

        Returns the last observed value either way (the caller asserts on
        it). The live sim is real-time, so settle-then-check with a
        tolerance rather than reading a single transient sample.
        """
        deadline = time.monotonic() + timeout.total_seconds()
        interval = poll.total_seconds()
        value = read()
        while not predicate(value) and time.monotonic() < deadline:
            time.sleep(interval)
            value = read()
        return value

    # --- lifecycle --------------------------------------------------------

    def close(self) -> None:
        # Under the same lock grpc_client() inserts with — a still-running
        # to_thread assertion must not slip a fresh client past the drain.
        with self._grpc_lock:
            for client in self._grpc_clients.values():
                client.close()
            self._grpc_clients.clear()
        self._http.close()
        terminate(self._process)
        # The launch tmpdir (rendered config + endpoints + log) is only
        # debris once its process is gone. `launch` failure paths never
        # reach here, so the log survives for post-mortem reading there.
        if self._tmpdir is not None:
            shutil.rmtree(self._tmpdir, ignore_errors=True)
            self._tmpdir = None

    def __enter__(self) -> Site:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


def _site_from_endpoints(
    endpoints: dict[str, Any],
    process: subprocess.Popen[bytes] | None,
    tmpdir: Path | None = None,
) -> Site:
    microgrids = {
        int(m["id"]): MicrogridEndpoint(
            id=int(m["id"]), name=m["name"], grpc=m["grpc_addr"]
        )
        for m in endpoints.get("microgrids", [])
    }
    return Site(
        ui=endpoints["ui"],
        microgrids=microgrids,
        assets=endpoints.get("assets"),
        dispatch=endpoints.get("dispatch"),
        process=process,
        tmpdir=tmpdir,
    )


def launch(
    config: LaunchConfig,
    *,
    bin: str | os.PathLike[str] | None = None,
    ready_timeout: timedelta = timedelta(seconds=20),
) -> Site:
    """Boot macrocosim on ephemeral ports and return a ready ``Site``.

    ``config`` is a path to a ``.lisp`` file, or a builder object with a
    ``to_lisp()`` method (a :class:`macrocosim.build.Microgrid`), which is
    rendered to a temp config first. Spawns ``macrocosim <config>
    --ephemeral-ports --emit-endpoints=<file>`` and blocks until the
    endpoints file is written (the readiness signal). Raises if the process
    dies first or the handshake times out.
    """
    spawned = spawn_macrocosim(config, bin)
    deadline = time.monotonic() + ready_timeout.total_seconds()
    while True:
        if spawned.endpoints_file.exists() and spawned.endpoints_file.stat().st_size > 0:
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
        time.sleep(0.1)

    # Any failure after the handshake must not leak the just-booted
    # simulator (mirrors the aio twin's finally-terminate).
    try:
        endpoints = json.loads(spawned.endpoints_file.read_text())
        # The binary boots (and reports ready) even when the config
        # registered no microgrid — a legitimate state for the
        # interactive bare engine, but a dead end for a client whose
        # config was supposed to build a topology. Fail fast with the
        # config's likely mistake (and the log tail, via fail()).
        if not endpoints.get("microgrids"):
            spawned.fail(
                RuntimeError,
                "macrocosim booted but the config registered no microgrids "
                "— does it call (make-microgrid …)?",
            )
        return _site_from_endpoints(endpoints, spawned.process, spawned.tmpdir)
    except BaseException:
        terminate(spawned.process)
        raise


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
