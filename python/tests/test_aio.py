"""The async core: routing, guards, and the generic expect surface.

No server needed — transports are monkeypatched. The live end-to-end
pass is in ``test_aio_live.py`` (skips without a simulator binary).
"""

from __future__ import annotations

from datetime import timedelta
from typing import Any

import httpx
import pytest
from frequenz.quantities import Energy, Power, ReactivePower

import macrocosim as mc
from macrocosim.errors import EvalRejected

_FAST = {"timeout": timedelta(seconds=0.2), "poll": timedelta(seconds=0.01)}


def _site(**latest: Any) -> mc.aio.Site:
    site = mc.aio.connect(
        ui="127.0.0.1:9",
        microgrids={1: mc.MicrogridEndpoint(id=1, name="a", grpc="10.0.0.1:61000")},
    )

    async def fake_latest(mg_id: int | None = None) -> dict[str, Any]:
        return {name: {"value": value} for name, value in latest.items()}

    site.latest = fake_latest  # type: ignore[method-assign]
    return site


def test_no_microgrids_raises_a_clear_error() -> None:
    site = mc.aio.connect(ui="127.0.0.1:9")
    with pytest.raises(RuntimeError, match="no microgrid endpoints"):
        _ = site.grpc


def test_grpc_url_prefixes_scheme() -> None:
    site = _site()
    assert site.grpc_url == "grpc://10.0.0.1:61000"


async def test_reads_map_metric_names_to_streams() -> None:
    # battery_* signals read the battery_pool_* streams; the mapping lives
    # in one table, not in each attribute.
    site = _site(battery_pool_power=-2000.0, battery_pool_energy=-50.0)
    assert await site.battery_power.try_read() == Power.from_watts(-2000.0)
    assert await site.battery_energy.try_read() == Energy.from_watt_hours(-50.0)
    assert await site.grid_power.try_read() is None  # stream absent -> None


async def test_signal_expect_settles_on_an_instantaneous_metric() -> None:
    site = _site(grid_power=100.0)
    value = await site.grid_power.expect(mc.at_most(Power.from_watts(200)), **_FAST)
    assert value == Power.from_watts(100.0)


async def test_signal_expect_checks_a_cumulative_metric_once() -> None:
    site = _site(grid_energy=50.0)
    value = await site.grid_energy.expect(
        mc.at_most(Energy.from_watt_hours(100)), **_FAST
    )
    assert value == Energy.from_watt_hours(50.0)


async def test_raw_handle_signals_read_the_component() -> None:
    site = _site()

    async def fake_active_power(cid: int, mg_id: int | None = None) -> Power | None:
        return Power.from_watts(float(cid))

    site.active_power = fake_active_power  # type: ignore[method-assign]
    value = await site[7].power.expect(mc.at_most(Power.from_watts(10)), **_FAST)
    assert value == Power.from_watts(7.0)
    # The raw-id handle's signals only observe (category unknown): no set.
    assert not hasattr(site[7].power, "set")


def test_component_handle_inherits_the_builders_microgrid() -> None:
    site = _site()
    bat = mc.battery(id=9)
    bat._bind(site, 2)  # what launch() does in a multi-microgrid config
    assert site[bat]._mg == 2
    # An explicit mg_id still wins; raw ids keep the default routing.
    assert site.component(bat, mg_id=1)._mg == 1
    assert site[9]._mg is None


async def test_microgrid_view_routes_the_mg_id() -> None:
    site = _site()
    seen: list[int | None] = []

    async def fake_latest(mg_id: int | None = None) -> dict[str, Any]:
        seen.append(mg_id)
        return {"grid_power": {"value": 100.0}}

    site.latest = fake_latest  # type: ignore[method-assign]
    assert await site.microgrid(2).grid_power.try_read() == Power.from_watts(100.0)
    assert await site.grid_power.try_read() == Power.from_watts(100.0)
    assert seen == [2, None]


async def test_drive_posts_typed_control_payloads() -> None:
    site = _site()
    calls: list[tuple[str, Any]] = []

    async def fake_control(path: str, payload: Any) -> Any:
        calls.append((path, payload))
        return {}

    site._http.control = fake_control  # type: ignore[method-assign]
    await site[6].drive(power=Power.from_kilowatts(20))
    assert calls == [("/api/mg/1/component/6/drive", {"power_w": 20000.0})]


def test_resolve_mg_defaults_to_the_lowest_id() -> None:
    site = mc.aio.connect(
        ui="127.0.0.1:9",
        microgrids={
            7: mc.MicrogridEndpoint(id=7, name="a", grpc="10.0.0.7:61000"),
            3: mc.MicrogridEndpoint(id=3, name="b", grpc="10.0.0.3:61000"),
        },
    )
    assert site._resolve_mg(None) == 3
    assert site._resolve_mg(7) == 7


async def test_meter_reactive_power_reads_and_drives_through_the_site() -> None:
    # Meter.reactive_power's string couplings — the async
    # Site.reactive_power reader and the drive payload key — are
    # pinned here so a typo in either fails a test instead of a live
    # session.
    site = _site()
    reads: list[int] = []

    async def fake_reactive_power(
        cid: int, mg_id: int | None = None
    ) -> ReactivePower | None:
        reads.append(cid)
        return ReactivePower.from_volt_amperes_reactive(750.0)

    site.reactive_power = fake_reactive_power  # type: ignore[method-assign]
    calls: list[tuple[Any, ...]] = []

    async def fake_control_component(
        cid: int, action: str, payload: Any, mg_id: int | None = None
    ) -> None:
        calls.append((cid, action, payload))

    site.control_component = fake_control_component  # type: ignore[method-assign]

    m = mc.meter(id=7, power=100.0)
    m._bind(site)
    assert await m.reactive_power.try_read() == ReactivePower.from_volt_amperes_reactive(
        750.0
    )
    assert reads == [7]
    await m.reactive_power.set(ReactivePower.from_kilo_volt_amperes_reactive(1.5))
    assert calls == [(7, "drive", {"reactive_var": 1500.0})]


async def test_rejected_eval_raises_from_raw_drive() -> None:
    # A RawLisp drive still goes through the eval choke point.
    site = _site()

    async def fake_eval(expr: str, mg_id: int | None = None) -> dict[str, Any]:
        return {"ok": False, "error": "set-meter-power: component 3 not found"}

    site._http.eval = fake_eval  # type: ignore[method-assign]
    with pytest.raises(EvalRejected, match="not found"):
        await site[3].drive(power=mc.raw("(lambda () 100.0)"))
    # EvalRejected still satisfies the historic except ValueError.
    assert issubclass(EvalRejected, ValueError)


class _FakeProcess:
    def __init__(self) -> None:
        self.terminated = False

    def poll(self) -> int | None:
        return 0 if self.terminated else None

    def terminate(self) -> None:
        self.terminated = True

    def wait(self, timeout: float | None = None) -> int:
        return 0


def _fake_spawn(tmp_path, monkeypatch) -> None:
    """Make aio.launch handshake instantly against no real process."""
    import macrocosim._process as proc_mod
    import macrocosim.aio._site as site_mod

    endpoints = tmp_path / "endpoints.json"
    endpoints.write_text(
        '{"ui": "127.0.0.1:9", "microgrids":'
        ' [{"id": 1, "name": "a", "grpc": "10.0.0.1:61000"}]}'
    )
    log = tmp_path / "macrocosim.log"
    log.write_text("")

    def spawn(config, bin):
        return proc_mod.SpawnedMacrocosim(
            process=_FakeProcess(),
            endpoints_file=endpoints,
            log_file=log,
            tmpdir=tmp_path,
        )

    monkeypatch.setattr(site_mod, "spawn_macrocosim", spawn)


async def test_launch_binds_and_unbinds_the_topology(tmp_path, monkeypatch) -> None:
    _fake_spawn(tmp_path, monkeypatch)
    load = mc.meter(id=5)
    mg = mc.Microgrid(id=1, topology=mc.grid(id=1, successors=[load]))
    async with mc.aio.launch(mg) as site:
        assert load._site is site
    assert load._site is None  # exit unbinds: the builder is a spec again


async def test_failed_bind_unbinds_the_partial_prefix(tmp_path, monkeypatch) -> None:
    _fake_spawn(tmp_path, monkeypatch)
    first = mc.meter(id=5)
    second = mc.meter(id=6)
    second._bind(object())  # pre-bound elsewhere: the 2nd bind will fail
    mg = mc.Microgrid(id=1, topology=mc.grid(id=1, successors=[first, second]))
    with pytest.raises(RuntimeError, match="already bound"):
        async with mc.aio.launch(mg):
            pytest.fail("launch must not yield")
    # The partial prefix was cleaned up: a retry starts fresh.
    assert first._site is None
    second._unbind()


async def test_scenario_run_fails_fast_without_a_length() -> None:
    site = _site()

    async def fake_get_json(path: str) -> Any:
        assert path == "/api/scenarios"
        return [{"name": "soak", "length_s": None}]

    site._http.get_json = fake_get_json  # type: ignore[method-assign]
    with pytest.raises(ValueError, match="no :length"):
        await site.scenario("soak").run(wait=True)


def _recording_posts(site: mc.aio.Site) -> list[str]:
    """Fake ``site``'s POSTs; returns the list the paths land in."""
    posts: list[str] = []

    async def fake_post(path: str, content: str = "") -> Any:
        posts.append(path)
        return {}

    site._http.post = fake_post  # type: ignore[method-assign]
    return posts


async def test_scenario_run_fails_fast_without_microgrids() -> None:
    site = mc.aio.connect(ui="127.0.0.1:9")
    posts = _recording_posts(site)
    with pytest.raises(RuntimeError, match="no microgrid endpoints"):
        await site.scenario("soak").run(wait=True, until=timedelta(seconds=1))
    assert posts == []  # never started → nothing to orphan


async def test_scenario_run_on_a_missing_microgrid_starts_nothing() -> None:
    site = _site()
    posts = _recording_posts(site)

    async def fake_get_json(path: str) -> Any:
        assert path == "/api/mg/1/scenario"
        request = httpx.Request("GET", f"http://ui{path}")
        response = httpx.Response(
            404, json={"error": "microgrid 1 not registered"}, request=request
        )
        raise httpx.HTTPStatusError("404 Not Found", request=request, response=response)

    site._http.get_json = fake_get_json  # type: ignore[method-assign]
    with pytest.raises(httpx.HTTPStatusError, match="404"):
        await site.scenario("soak").run(wait=True, until=timedelta(seconds=1))
    assert posts == []  # never started → nothing to orphan


async def test_scenario_run_stops_when_a_poll_raises() -> None:
    site = _site()
    posts = _recording_posts(site)
    reads: list[str] = []

    async def fake_get_json(path: str) -> Any:
        assert path == "/api/mg/1/scenario"
        reads.append(path)
        # The first read is the check before the start; the next one
        # is the first poll.
        if len(reads) > 1:
            raise httpx.ReadTimeout("poll timed out")
        return {"name": "soak", "ended_at": None, "elapsed_s": 0.0}

    site._http.get_json = fake_get_json  # type: ignore[method-assign]
    with pytest.raises(httpx.ReadTimeout, match="poll timed out"):
        await site.scenario("soak").run(wait=True, until=timedelta(seconds=1))
    assert posts == ["/api/scenarios/soak/start", "/api/scenarios/stop"]


async def test_scenario_wait_requires_a_length() -> None:
    site = _site()

    async def fake_get_json(path: str) -> Any:
        return [{"name": "soak", "length_s": None}]

    site._http.get_json = fake_get_json  # type: ignore[method-assign]
    with pytest.raises(ValueError, match="no :length"):
        await site.scenario("soak").wait()


async def test_scenario_assert_passed_returns_report_when_clean() -> None:
    site = _site()

    async def fake_get_json(path: str) -> Any:
        assert path == "/api/mg/1/scenario/report"
        return {"name": "s", "checks_passed": 2, "checks_failed": 0, "checks": []}

    site._http.get_json = fake_get_json  # type: ignore[method-assign]
    report = await site.scenario("s").assert_passed()
    assert report["checks_passed"] == 2


async def test_scenario_assert_passed_raises_when_no_check_ran() -> None:
    site = _site()

    async def fake_get_json(path: str) -> Any:
        assert path == "/api/mg/1/scenario/report"
        return {"name": "s", "checks_passed": 0, "checks_failed": 0, "checks": []}

    site._http.get_json = fake_get_json  # type: ignore[method-assign]
    with pytest.raises(AssertionError, match="'s'.*no check ran"):
        await site.scenario("s").assert_passed()
