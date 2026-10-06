"""Run registered Lisp scenarios from Python and gate on their checks.

A scenario is defined in the loaded config with ``(define-scenario …)``;
reach it via ``site.scenario(name)``:

    report = site.scenario("cloud-fade").run(wait=True).assert_passed()

``run(wait=True)`` starts it live, blocks until it finishes (its
``:length`` or ``until=``), then stops it so the report freezes — the same
sequence as ``macroctl scenario run --wait``. ``report()`` returns the parsed
pass/fail ledger + stats; ``assert_passed()`` raises on any failed
``(check …)``, or if none ran; ``events()`` reads the journal.

Times are :mod:`datetime` (a ``timedelta`` offset, or a ``datetime.time``
for an absolute schedule); check values are ``frequenz-quantities``.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from datetime import time as clock_time
from datetime import timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any, TypeAlias

from ._http import journal_path
from ._process import render_config, resolve_binary
from .build import Component, ConfigSource, RawLisp, to_lisp_atom
from .enums import Metric, Schedule
from .matchers import Matcher

if TYPE_CHECKING:
    from frequenz.quantities import Power

    from ._http import HttpClient
    from .runtime import Site
    from .signals import DrivenSignal, SettingSignal, Signal

# A scenario report / journal are dynamic JSON from macrocosim; named for intent.
ScenarioReport: TypeAlias = dict[str, Any]
JournalEvent: TypeAlias = dict[str, Any]


def _time_literal(value: timedelta | clock_time) -> str:
    """A scenario time as Lisp: a ``timedelta`` becomes a ``"<seconds>s"`` offset,
    a ``datetime.time`` an ``"HH:MM:SS"`` clock string (for absolute schedules).

    Whole seconds render without a decimal point, fractional as a plain decimal
    — never scientific notation, since macrocosim's ``parse-offset`` splits at
    the first letter and would read an ``e``-exponent as the unit.
    """
    if isinstance(value, clock_time):
        return f'"{value.isoformat()}"'
    secs = value.total_seconds()
    if secs == int(secs):
        text = f"{secs:.0f}"
    else:
        # Fixed-point, then strip trailing zeros: repr() would render
        # sub-100 µs offsets as e.g. "5e-05". 6 decimals cover timedelta's
        # microsecond resolution.
        text = f"{secs:.6f}".rstrip("0")
    return f'"{text}s"'


def _assert_report_passed(name: str, report: ScenarioReport) -> None:
    """Raise unless the report shows at least one check and no failures."""
    failed = report.get("checks_failed", 0)
    if failed:
        broken = [c for c in report.get("checks", []) if not c.get("passed", True)]
        raise AssertionError(f"scenario {name!r}: {failed} check(s) failed: {broken}")
    # The failed branch above already returned, so failed == 0 here.
    if not report.get("checks_passed", 0):
        raise AssertionError(
            f"scenario {name!r}: no check ran — the scenario ended "
            "before its first (check …) fired, so nothing was verified"
        )


@dataclass
class Check:
    """A timed ``(check …)`` assertion within a scenario."""

    at: timedelta | clock_time
    component: int
    metric: str
    matcher: Matcher[Any]

    def to_lisp(self) -> str:
        parts = [
            _time_literal(self.at),
            f":component {self.component}",
            f":metric '{self.metric}",
        ]
        m = self.matcher
        if m.approx is not None:
            parts.append(f":approx {to_lisp_atom(m.approx)}")
        if m.tol is not None:
            parts.append(f":tol {to_lisp_atom(m.tol)}")
        if m.within is not None:
            # The server check has no :within; a closed interval is
            # exactly :min + :max.
            low, high = m.within
            parts.append(f":min {to_lisp_atom(low)}")
            parts.append(f":max {to_lisp_atom(high)}")
        if m.min is not None:
            parts.append(f":min {to_lisp_atom(m.min)}")
        if m.max is not None:
            parts.append(f":max {to_lisp_atom(m.max)}")
        return f"(check {' '.join(parts)})"


@dataclass
class Scenario:
    """Author a scenario in Python; renders to ``(define-scenario …)``.

    Register it on a running site with ``site.define_scenario(scenario)``,
    which returns a :class:`ScenarioRun` to ``run(...)`` and gate. The DSL
    speaks signals, like the live API: ``check`` schedules an assertion on
    a component signal with one typed matcher, ``at`` schedules a cue that
    sets a signal to a value, and ``drive_meter`` installs a continuous
    meter source. ``check_metric`` covers server metrics without a signal
    yet (per-component energy); ``drive`` / ``cue`` splice raw Lisp
    ``(drive-* …)`` / ``(at …)`` forms for anything else.
    """

    name: str
    schedule: Schedule = Schedule.RELATIVE
    length: timedelta | None = None
    seed: int | None = None
    description: str | None = None
    _checks: list[Check] = field(default_factory=list)
    _drives: list[str] = field(default_factory=list)
    _cues: list[str] = field(default_factory=list)

    def check(
        self,
        at: timedelta | clock_time,
        signal: Signal[Any],
        matcher: Matcher[Any],
    ) -> Scenario:
        """Schedule an assertion on a component signal (``power``, ``soc``).

        The same matchers as the live ``expect``; the signal only lends
        its identity, so an unbound builder's signal works here.
        """
        component_id, metric = signal._scenario_check_ref()
        self._checks.append(Check(at, component_id, metric, matcher))
        return self

    def check_metric(
        self,
        at: timedelta | clock_time,
        *,
        component: int,
        metric: Metric,
        matcher: Matcher[Any],
    ) -> Scenario:
        """Schedule a check on a server metric with no signal yet.

        The escape hatch for per-component ``Metric.ENERGY`` (and other
        wire metrics) until they grow signals.
        """
        self._checks.append(Check(at, component, metric.value, matcher))
        return self

    def at(
        self,
        when: timedelta | clock_time,
        target: DrivenSignal[Any] | SettingSignal[Any],
        value: Any,
    ) -> Scenario:
        """Schedule a cue: set ``target`` to ``value`` at ``when``.

        Any settable signal works — a meter's ``power``, a PV's
        ``sunlight``, a battery's ``soc``, a component's ``health``.
        """
        cue = getattr(target, "_scenario_cue", None)
        if cue is None:
            label = getattr(target, "_label", repr(target))
            raise TypeError(
                f"{label} cannot be cued: it is read-only. "
                "Cue a settable signal (a meter's power, a PV's sunlight, "
                "a battery's soc, a component's health)."
            )
        # The prelude's `at` takes a THUNK: without the lambda the action
        # would run eagerly when the cue list is evaluated (at define
        # time), not at its scheduled offset.
        self._cues.append(f"(at {_time_literal(when)} (lambda () {cue(value)}))")
        return self

    def drive_meter(self, meter: Component | int, value: Power | RawLisp) -> Scenario:
        """Install a continuous source on a meter (a value, or ``raw`` Lisp)."""
        cid = meter.component_id if isinstance(meter, Component) else int(meter)
        self._drives.append(f"(drive-meter {cid} {to_lisp_atom(value)})")
        return self

    def drive(self, form: str | RawLisp) -> Scenario:
        """Splice a raw ``(drive-* …)`` form (e.g. drive-solar with a timeline)."""
        self._drives.append(form.text if isinstance(form, RawLisp) else form)
        return self

    def cue(self, at: timedelta | clock_time, action: str | RawLisp) -> Scenario:
        """A timed cue: ``(at "<time>" <action>)``.

        ``action`` must evaluate to a 0-arg lambda (a thunk) — e.g.
        ``(event ...)`` returns one. A bare call would run at define
        time; wrap it: ``(lambda () (...))``.
        """
        text = action.text if isinstance(action, RawLisp) else action
        self._cues.append(f"(at {_time_literal(at)} {text})")
        return self

    def to_lisp(self) -> str:
        parts = [f":name {to_lisp_atom(self.name)}"]
        if self.description is not None:
            parts.append(f":description {to_lisp_atom(self.description)}")
        parts.append(f":schedule {to_lisp_atom(self.schedule)}")
        if self.length is not None:
            parts.append(f":length {_time_literal(self.length)}")
        if self.seed is not None:
            parts.append(f":seed {self.seed}")
        if self._drives:
            parts.append(f":drive (list {' '.join(self._drives)})")
        if self._cues:
            parts.append(f":cues (list {' '.join(self._cues)})")
        if self._checks:
            checks = " ".join(c.to_lisp() for c in self._checks)
            parts.append(f":expect (list {checks})")
        return f"(define-scenario {' '.join(parts)})"


class ScenarioRun:
    """A registered scenario bound to a running site."""

    def __init__(self, site: Site, name: str) -> None:
        self._site = site
        self._name = name

    @property
    def _http(self) -> HttpClient:
        return self._site._http

    def _length_s(self) -> float | None:
        for scenario in self._http.get_json("/api/scenarios"):
            if scenario.get("name") == self._name:
                return scenario.get("length_s")
        return None

    def run(
        self,
        *,
        wait: bool = True,
        until: timedelta | None = None,
        poll: timedelta = timedelta(seconds=1),
    ) -> ScenarioRun:
        """Start the scenario; with ``wait`` block until it finishes, stop it."""
        length = until.total_seconds() if until is not None else self._length_s()
        # Resolve the wait length BEFORE starting, so an unwaitable scenario
        # fails fast instead of being left running with nothing to stop it.
        if wait and length is None:
            raise ValueError(
                f"scenario {self._name!r} has no :length; pass until= to bound the wait"
            )
        self._http.post(f"/api/scenarios/{self._name}/start")
        if not wait or length is None:
            return self
        return self._wait_for(length, poll)

    def wait(
        self,
        *,
        until: timedelta | None = None,
        poll: timedelta = timedelta(seconds=1),
    ) -> ScenarioRun:
        """Block until the running scenario finishes, then stop it.

        The companion to ``run(wait=False)`` — start the scenario, do
        other work, then wait for the report.
        """
        length = until.total_seconds() if until is not None else self._length_s()
        if length is None:
            raise ValueError(
                f"scenario {self._name!r} has no :length; pass until= to bound the wait"
            )
        return self._wait_for(length, poll)

    def _assert_active(self, state: ScenarioReport) -> None:
        # The server tracks one scenario; a mismatched (or absent) name
        # means this run never started or another scenario's state is
        # live — waiting on it, or judging its report, would silently
        # test the wrong thing.
        if state.get("name") != self._name:
            raise RuntimeError(
                f"scenario {self._name!r} is not the active scenario "
                f"(server reports {state.get('name')!r}); was run() called?"
            )

    def _wait_for(self, length: float, poll: timedelta) -> ScenarioRun:
        deadline = time.monotonic() + length + 5.0
        interval = poll.total_seconds()
        while time.monotonic() < deadline:
            state = self._http.get_json(journal_path(self._site._resolve_mg(None)))
            self._assert_active(state)
            if state.get("ended_at") is not None:
                break
            if (state.get("elapsed_s") or 0.0) >= length:
                break
            time.sleep(interval)
        self._http.post("/api/scenarios/stop")
        return self

    def report(self) -> ScenarioReport:
        """The parsed scenario report (pass/fail ledger + peak/soc stats)."""
        # The report carries the scenario name it belongs to; checking
        # it in the same response avoids a two-request race.
        report = self._http.get_json(
            journal_path(self._site._resolve_mg(None), "/report")
        )
        self._assert_active(report)
        return report

    def assert_passed(self) -> ScenarioReport:
        """Raise if any ``(check …)`` failed, or if none ran; return the report
        otherwise."""
        report = self.report()
        _assert_report_passed(self._name, report)
        return report

    def events(self, *, since: int = 0) -> list[JournalEvent]:
        """The scenario's journal events (list of ``{kind, payload, …}``)."""
        body = self._http.get_json(
            journal_path(self._site._resolve_mg(None), f"/events?since={since}")
        )
        return body.get("events", [])


def run_scenario_stepped(
    config: ConfigSource,
    name: str,
    *,
    macroctl_bin: str | os.PathLike[str] | None = None,
    until: timedelta | None = None,
    step: int | None = None,
    assert_pass: bool = True,
) -> ScenarioReport:
    """Run a scenario headless on the stepped clock and return its report.

    Deterministic and faster than real time — no server, no app under test
    (e2e-testing.md mode 1). ``config`` is a ``.lisp`` path, or builder
    object(s) (a ``Microgrid`` and ``Scenario``) rendered to a temp config.
    Shells ``macroctl scenario run NAME --stepped --config … --json``; with
    ``assert_pass`` a non-zero exit (a failed ``(check …)``), or a report
    with no checks run, raises.
    """
    binary = resolve_binary(
        "macroctl", env_var="MACROCTL_BIN", explicit=macroctl_bin, flag="macroctl_bin"
    )
    # Only builder objects need rendering to a temp file; a .lisp path
    # is used as-is, and minting (and leaking) a tmpdir for it would
    # accumulate one empty directory per run.
    tmpdir: Path | None = None
    try:
        if isinstance(config, (str, os.PathLike)):
            # Same spurious Protocol-and-PathLike intersection
            # render_config works around; the runtime narrowing is
            # str | PathLike here.
            config_path = Path(config)  # ty: ignore[invalid-argument-type]
        else:
            tmpdir = Path(tempfile.mkdtemp(prefix="macrocosim-py-"))
            config_path = render_config(config, tmpdir)
        args = [
            binary,
            "scenario",
            "run",
            name,
            "--stepped",
            "--config",
            str(config_path),
            "--json",
        ]
        if until is not None:
            args += ["--until", str(int(until.total_seconds()))]
        if step is not None:
            args += ["--step", str(int(step))]
        if assert_pass:
            args += ["--assert"]

        result = subprocess.run(args, capture_output=True, text=True, check=False)
    finally:
        # The rendered config has served its purpose; failure diagnostics
        # come from macroctl's captured output, not the tmpdir — clean up
        # even when rendering or the run itself raises (bad builder,
        # Ctrl-C, exec failure).
        if tmpdir is not None:
            shutil.rmtree(tmpdir, ignore_errors=True)
    report: ScenarioReport | None
    try:
        report = json.loads(result.stdout)
    except json.JSONDecodeError:
        report = None
    if assert_pass and result.returncode != 0:
        broken = (
            [c for c in report.get("checks", []) if not c.get("passed", True)]
            if report
            else result.stderr.strip()
        )
        raise AssertionError(
            f"stepped scenario {name!r} failed (exit {result.returncode}): {broken}"
        )
    if report is None:
        raise RuntimeError(f"macroctl produced no JSON report:\n{result.stderr}")
    if assert_pass:
        _assert_report_passed(name, report)
    return report


__all__ = [
    "Check",
    "Scenario",
    "ScenarioRun",
    "ScenarioReport",
    "JournalEvent",
    "run_scenario_stepped",
]
