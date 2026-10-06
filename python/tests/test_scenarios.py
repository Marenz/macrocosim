"""ScenarioRun tests — a fake HTTP site, no binary."""

from __future__ import annotations

from datetime import timedelta

import httpx
import pytest
from frequenz.quantities import Power

import macrocosim.scenarios as scenarios_mod
from macrocosim.build import meter as mc_meter
from macrocosim.enums import Metric, Schedule
from macrocosim.matchers import at_least, at_most, near
from macrocosim.scenarios import Scenario, ScenarioRun, run_scenario_stepped


class _FakeProc:
    def __init__(self, returncode: int, stdout: str) -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = ""


class FakeHttp:
    def __init__(self, report: dict) -> None:
        # Real reports carry the scenario name; the client checks it.
        self._report = {"name": "s", **report}
        self.posts: list[str] = []
        self.gets: list[str] = []

    def get_json(self, path: str):
        if path == "/api/scenarios":
            return [{"name": "s", "length_s": 0.0}]
        self.gets.append(path)
        if path == "/api/mg/3/scenario":
            return {"name": "s", "ended_at": "2026-01-01T00:00:00Z", "elapsed_s": 1.0}
        if path == "/api/mg/3/scenario/report":
            return self._report
        if path.startswith("/api/mg/3/scenario/events"):
            return {"events": [{"kind": "note", "payload": "hi"}], "next_event_id": 1}
        raise AssertionError(f"unexpected GET {path}")

    def post(self, path: str, content: str = "") -> dict:
        self.posts.append(path)
        return {}


class FakeSite:
    """A site with microgrids 7 and 3: readouts go to the lowest."""

    def __init__(self, report: dict) -> None:
        self._http = FakeHttp(report)
        self.microgrids = {7: object(), 3: object()}

    def _resolve_mg(self, mg_id: int | None) -> int:
        return mg_id if mg_id is not None else min(self.microgrids)


METER = mc_meter(id=2)


def test_check_metric_is_the_escape_for_signal_less_metrics() -> None:
    scn = Scenario("e", length=timedelta(seconds=60)).check_metric(
        timedelta(seconds=30),
        component=2,
        metric=Metric.ENERGY,
        matcher=at_most(Power.from_watts(70)),
    )
    assert '(check "30s" :component 2 :metric \'energy :max 70.0)' in scn.to_lisp()


def test_cues_render_from_settable_signals() -> None:
    from frequenz.quantities import Percentage

    from macrocosim.build import battery, solar_inverter

    bat = battery(id=4)
    pv = solar_inverter(id=8)
    scn = Scenario("c", length=timedelta(seconds=60))
    scn.at(timedelta(seconds=5), METER.power, Power.from_kilowatts(20))
    scn.at(timedelta(seconds=10), bat.soc, Percentage.from_percent(11))
    scn.at(timedelta(seconds=15), pv.sunlight, Percentage.from_percent(80))
    lisp = scn.to_lisp()
    # Cue actions are thunks: the lambda defers them to their offset.
    assert '(at "5s" (lambda () (set-meter-power 2 20000.0)))' in lisp
    assert '(at "10s" (lambda () (set-battery-soc 4 11.0)))' in lisp
    assert '(at "15s" (lambda () (set-solar-sunlight 8 80.0)))' in lisp


def test_aggregate_signals_are_not_checkable() -> None:
    import macrocosim as mc

    site = mc.aio.connect(
        ui="127.0.0.1:9",
        microgrids={1: mc.MicrogridEndpoint(id=1, name="a", grpc="10.0.0.1:1")},
    )
    scn = Scenario("x", length=timedelta(seconds=1))
    with pytest.raises(ValueError, match="component signals"):
        scn.check(timedelta(seconds=1), site.grid_power, at_most(Power.zero()))


def test_run_starts_and_stops() -> None:
    site = FakeSite({"checks_passed": 1, "checks_failed": 0, "checks": []})
    ScenarioRun(site, "s").run(wait=True)
    assert site._http.posts == ["/api/scenarios/s/start", "/api/scenarios/stop"]


def test_at_rejects_a_read_only_signal() -> None:
    from macrocosim.build import battery_inverter

    inv = battery_inverter(id=3)
    scn = Scenario("s")
    with pytest.raises(TypeError, match="cannot be cued"):
        scn.at(timedelta(seconds=0), inv.power, Power.from_kilowatts(5))


def test_wait_and_report_reject_an_inactive_scenario() -> None:
    class OtherActiveHttp(FakeHttp):
        def get_json(self, path: str):
            # No scenario ever started: summary and report are empty.
            if path == "/api/mg/3/scenario":
                return {"name": None, "ended_at": None, "elapsed_s": 0.0}
            if path == "/api/mg/3/scenario/report":
                return {"name": None, "checks_passed": 0, "checks_failed": 0}
            return super().get_json(path)

    site = FakeSite({})
    site._http = OtherActiveHttp({})
    with pytest.raises(RuntimeError, match="not the active scenario"):
        ScenarioRun(site, "s").wait(until=timedelta(seconds=1))
    with pytest.raises(RuntimeError, match="not the active scenario"):
        ScenarioRun(site, "s").report()
    # The stop route takes no name: posting it would stop the active
    # scenario, which is not this one.
    assert site._http.posts == []


def test_run_wait_without_length_raises_before_starting() -> None:
    class NoLengthHttp(FakeHttp):
        def get_json(self, path: str):
            if path == "/api/scenarios":
                return [{"name": "s"}]  # no length_s
            return super().get_json(path)

    site = FakeSite({})
    site._http = NoLengthHttp({})
    with pytest.raises(ValueError, match="no :length"):
        ScenarioRun(site, "s").run(wait=True)
    assert site._http.posts == []  # never started → nothing to orphan


def test_run_wait_without_microgrids_raises_before_starting() -> None:
    import macrocosim as mc

    site = mc.connect(ui="127.0.0.1:9")
    http = FakeHttp({})
    site._http = http  # type: ignore[assignment]
    with pytest.raises(RuntimeError, match="no microgrid endpoints"):
        ScenarioRun(site, "s").run(wait=True)
    assert http.posts == []  # never started → nothing to orphan


def _not_found(path: str) -> httpx.HTTPStatusError:
    """The error ``HttpClient.get_json`` raises on a 404 for ``path``."""
    request = httpx.Request("GET", f"http://ui{path}")
    response = httpx.Response(
        404, json={"error": "microgrid 3 not registered"}, request=request
    )
    return httpx.HTTPStatusError("404 Not Found", request=request, response=response)


def test_run_wait_on_a_missing_microgrid_raises_before_starting() -> None:
    class MissingMgHttp(FakeHttp):
        def get_json(self, path: str):
            if path == "/api/mg/3/scenario":
                raise _not_found(path)
            return super().get_json(path)

    site = FakeSite({})
    site._http = MissingMgHttp({})
    with pytest.raises(httpx.HTTPStatusError, match="404"):
        ScenarioRun(site, "s").run(wait=True)
    assert site._http.posts == []  # never started → nothing to orphan


def test_run_wait_stops_the_scenario_when_a_poll_raises() -> None:
    class FailingPollHttp(FakeHttp):
        def get_json(self, path: str):
            # The first journal read is the check before the start;
            # the next one is the first poll.
            if path == "/api/mg/3/scenario" and path in self.gets:
                raise httpx.ReadTimeout("poll timed out")
            return super().get_json(path)

    site = FakeSite({})
    site._http = FailingPollHttp({})
    with pytest.raises(httpx.ReadTimeout, match="poll timed out"):
        ScenarioRun(site, "s").run(wait=True)
    assert site._http.posts == ["/api/scenarios/s/start", "/api/scenarios/stop"]


def test_a_failed_stop_after_a_failed_poll_raises_the_poll_error() -> None:
    class FailingPollAndStopHttp(FakeHttp):
        def get_json(self, path: str):
            # The first journal read is the check before the start;
            # the next one is the first poll.
            if path == "/api/mg/3/scenario" and path in self.gets:
                raise httpx.ReadTimeout("poll timed out")
            return super().get_json(path)

        def post(self, path: str, content: str = "") -> dict:
            super().post(path, content)
            if path == "/api/scenarios/stop":
                raise httpx.ConnectError("stop refused")
            return {}

    site = FakeSite({})
    site._http = FailingPollAndStopHttp({})
    with pytest.raises(httpx.ReadTimeout, match="poll timed out") as caught:
        ScenarioRun(site, "s").run(wait=True)
    assert site._http.posts == ["/api/scenarios/s/start", "/api/scenarios/stop"]
    assert any("stop refused" in note for note in caught.value.__notes__)


def test_a_failed_stop_after_a_clean_wait_raises_the_stop_error() -> None:
    class FailingStopHttp(FakeHttp):
        def post(self, path: str, content: str = "") -> dict:
            super().post(path, content)
            if path == "/api/scenarios/stop":
                raise httpx.ConnectError("stop refused")
            return {}

    site = FakeSite({})
    site._http = FailingStopHttp({})
    with pytest.raises(httpx.ConnectError, match="stop refused"):
        ScenarioRun(site, "s").run(wait=True)
    assert site._http.posts == ["/api/scenarios/s/start", "/api/scenarios/stop"]


def test_readouts_after_a_run_use_the_microgrid_it_picked() -> None:
    site = FakeSite({"checks_passed": 1, "checks_failed": 0, "checks": []})
    run = ScenarioRun(site, "s").run(wait=True)
    site.microgrids[1] = object()  # a lower microgrid appears afterwards
    run.report()
    run.events()
    assert site._http.gets[-2:] == [
        "/api/mg/3/scenario/report",
        "/api/mg/3/scenario/events?since=0",
    ]


def test_assert_passed_returns_report_when_clean() -> None:
    site = FakeSite({"checks_passed": 2, "checks_failed": 0, "checks": []})
    report = ScenarioRun(site, "s").assert_passed()
    assert report["checks_passed"] == 2


def test_assert_passed_raises_when_no_check_ran() -> None:
    site = FakeSite({"checks_passed": 0, "checks_failed": 0, "checks": []})
    with pytest.raises(AssertionError, match="'s'.*no check ran"):
        ScenarioRun(site, "s").assert_passed()


def test_assert_passed_raises_on_failure() -> None:
    report = {
        "checks_passed": 0,
        "checks_failed": 1,
        "checks": [{"passed": False, "detail": "too high"}],
    }
    with pytest.raises(AssertionError, match="1 check\\(s\\) failed"):
        ScenarioRun(FakeSite(report), "s").assert_passed()


def test_events_unwraps_list() -> None:
    events = ScenarioRun(FakeSite({}), "s").events()
    assert events == [{"kind": "note", "payload": "hi"}]


def test_readouts_carry_the_lowest_microgrid_id() -> None:
    site = FakeSite({"checks_passed": 1, "checks_failed": 0, "checks": []})
    run = ScenarioRun(site, "s")
    run.wait(until=timedelta(seconds=1))
    run.report()
    run.events(since=2)
    assert site._http.gets == [
        "/api/mg/3/scenario",
        "/api/mg/3/scenario/report",
        "/api/mg/3/scenario/events?since=2",
    ]


def test_scenario_authoring_emits_define_scenario() -> None:
    scn = (
        Scenario(
            "s",
            schedule=Schedule.RELATIVE,
            length=timedelta(minutes=4),
            seed=42,
            description="d",
        )
        .check(
            timedelta(seconds=1),
            METER.power,
            near(Power.from_watts(5000), tol=Power.from_watts(500)),
        )
        .drive_meter(METER, Power.from_megawatts(2))
        .cue(timedelta(seconds=60), '(event \'clouds "rolling in")')
    )
    lisp = scn.to_lisp()
    assert lisp.startswith(
        '(define-scenario :name "s" :description "d" :schedule \'relative'
    )
    assert ':length "240s"' in lisp
    assert ":seed 42" in lisp
    assert (
        '(check "1s" :component 2 :metric \'active-power :approx 5000.0 :tol 500.0)'
        in lisp
    )
    assert ":drive (list (drive-meter 2 2000000.0))" in lisp
    assert ':cues (list (at "60s" (event \'clouds "rolling in")))' in lisp


def test_run_scenario_stepped_returns_report(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(scenarios_mod, "resolve_binary", lambda *a, **k: "macroctl")
    monkeypatch.setattr(
        scenarios_mod.subprocess,
        "run",
        lambda *a, **k: _FakeProc(0, '{"checks_passed": 1, "checks_failed": 0}'),
    )
    report = run_scenario_stepped(str(tmp_path / "c.lisp"), "s")
    assert report["checks_passed"] == 1


def test_run_scenario_stepped_writes_seconds_flags(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(scenarios_mod, "resolve_binary", lambda *a, **k: "macroctl")
    seen: list[list[str]] = []

    def fake_run(args, **kwargs):
        seen.append(args)
        return _FakeProc(0, '{"checks_passed": 1, "checks_failed": 0}')

    monkeypatch.setattr(scenarios_mod.subprocess, "run", fake_run)
    run_scenario_stepped(
        str(tmp_path / "c.lisp"), "s", until=timedelta(seconds=90.5), step=250
    )
    args = seen[0]
    assert args[args.index("--until-s") + 1] == "90.5"
    assert args[args.index("--step-s") + 1] == "0.25"
    assert "--until" not in args
    assert "--step" not in args


def test_run_scenario_stepped_raises_when_no_check_ran(monkeypatch, tmp_path) -> None:
    # macroctl's --assert exits 0 when nothing ran; the zero-check counts
    # must be caught even though the exit code alone looks clean.
    monkeypatch.setattr(scenarios_mod, "resolve_binary", lambda *a, **k: "macroctl")
    monkeypatch.setattr(
        scenarios_mod.subprocess,
        "run",
        lambda *a, **k: _FakeProc(0, '{"checks_passed": 0, "checks_failed": 0}'),
    )
    with pytest.raises(AssertionError, match="'s'.*no check ran"):
        run_scenario_stepped(str(tmp_path / "c.lisp"), "s")


def test_run_scenario_stepped_raises_on_nonzero_exit(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(scenarios_mod, "resolve_binary", lambda *a, **k: "macroctl")
    monkeypatch.setattr(
        scenarios_mod.subprocess,
        "run",
        lambda *a, **k: _FakeProc(
            1, '{"checks_failed": 1, "checks": [{"passed": false, "metric": "x"}]}'
        ),
    )
    with pytest.raises(AssertionError, match="failed"):
        run_scenario_stepped(str(tmp_path / "c.lisp"), "s")


def test_scenario_name_escaped_and_time_non_scientific() -> None:
    scn = Scenario('a"b', length=timedelta(days=30)).check(
        timedelta(seconds=1),
        METER.power,
        near(Power.from_watts(1), tol=Power.from_watts(1)),
    )
    lisp = scn.to_lisp()
    assert '\\"' in lisp  # the embedded quote was escaped
    assert "2592000s" in lisp  # 30 days as plain integer seconds
    assert "e+" not in lisp  # never scientific notation


def test_sub_100_microsecond_offsets_render_fixed_point() -> None:
    # repr(5e-05) is scientific notation, which the server's parse-offset
    # rejects — fractional offsets must render as plain decimals.
    scn = Scenario("s", length=timedelta(seconds=1)).check(
        timedelta(microseconds=50),
        METER.power,
        at_least(Power.from_watts(0)),
    )
    lisp = scn.to_lisp()
    assert '"0.00005s"' in lisp
    assert "e-05" not in lisp
