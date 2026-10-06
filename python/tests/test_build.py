"""Emitter tests for the topology builder — pure, no binary required."""

from __future__ import annotations

import inspect
import re
from datetime import time, timedelta
from typing import Any

import pytest
from frequenz.quantities import (
    ApparentPower,
    Current,
    Energy,
    Percentage,
    Power,
    ReactivePower,
    Voltage,
)

import macrocosim as mc
from macrocosim.build import (
    Microgrid,
    battery,
    battery_inverter,
    grid,
    meter,
    plug_ev_form,
    raw,
)


def test_nested_successors_emit_make_forms() -> None:
    mg = Microgrid(
        id=1,
        topology=grid(id=1, successors=[meter(id=2, power=Power.from_watts(7000))]),
    )
    lisp = mg.to_lisp()
    assert "(make-microgrid :id 1" in lisp
    assert "(make-grid-connection-point :id 1 :successors (list" in lisp
    assert "(make-meter :id 2 :power-w 7000.0)" in lisp


def test_ergonomic_kwargs_map_to_plist_keys() -> None:
    # rated (a Power pair) → two keys in watts (inverters carry rated bounds).
    inv = battery_inverter(
        id=3, rated=(Power.from_kilowatts(-5), Power.from_kilowatts(5))
    )
    inv_lisp = inv.to_lisp()
    assert ":rated-lower-w -5000.0" in inv_lisp
    assert ":rated-upper-w 5000.0" in inv_lisp
    # Energy → Wh; Percentage → percent; soc → initial-soc (battery conveniences).
    bat_lisp = battery(
        id=4,
        capacity=Energy.from_kilowatt_hours(100),
        initial_soc=Percentage.from_percent(50),
        rated=(Power.from_kilowatts(-30), Power.from_kilowatts(30)),
    ).to_lisp()
    assert ":capacity-wh 100000.0" in bat_lisp
    assert ":initial-soc-pct 50.0" in bat_lisp
    assert ":rated-lower-w -30000.0" in bat_lisp
    assert ":rated-upper-w 30000.0" in bat_lisp


def test_value_kinds_render_correctly() -> None:
    # symbol-valued key (enum), string-valued key, bool → t, and the
    # interval timedelta lands as seconds.
    node = meter(
        id=2,
        name="main",
        hidden=True,
        health=mc.Health.ERROR,
        interval=timedelta(seconds=5),
    )
    lisp = node.to_lisp()
    assert ':name "main"' in lisp
    assert ":hidden t" in lisp
    assert ":health 'error" in lisp  # symbol, not a string
    assert ":interval-s 5.0" in lisp


def test_meter_reactive_kwargs_render_correctly() -> None:
    # A constant VAr load renders as :reactive-power-var with the VAr value.
    lisp = meter(
        id=2, reactive_power=ReactivePower.from_kilo_volt_amperes_reactive(1.5)
    ).to_lisp()
    assert ":reactive-power-var 1500.0" in lisp

    # The power-factor form renders the ratio plus the leading flag.
    lisp = meter(id=3, power_factor=0.8, leading=True).to_lisp()
    assert ":power-factor 0.8" in lisp
    assert ":leading t" in lisp

    # leading defaults to absent, not `nil`, when not asked for.
    lisp = meter(id=4, power_factor=0.9).to_lisp()
    assert ":power-factor 0.9" in lisp
    assert ":leading" not in lisp


def test_live_signal_path_exposes_reactive_power() -> None:
    """`Meter.reactive_power` reads through the bound *async* Site.

    `_bind` is only ever called with `macrocosim.aio.Site`, so the read
    path needs the async twins — the sync ones alone leave the signal
    raising AttributeError on first use.
    """
    from macrocosim.aio._grpc import AsyncGrpcClient
    from macrocosim.aio._site import Site as AsyncSite
    from macrocosim.runtime import Site as SyncSite

    for holder in (AsyncSite, AsyncGrpcClient, SyncSite):
        assert hasattr(holder, "active_power"), holder
        assert hasattr(holder, "reactive_power"), holder

    assert inspect.iscoroutinefunction(AsyncSite.reactive_power)
    assert inspect.iscoroutinefunction(AsyncGrpcClient.reactive_power)


def test_to_lisp_atom_converts_typed_values() -> None:
    assert mc.to_lisp_atom(Power.from_kilowatts(2)) == "2000.0"
    assert mc.to_lisp_atom(Energy.from_kilowatt_hours(1)) == "1000.0"
    assert mc.to_lisp_atom(Percentage.from_percent(50)) == "50.0"
    assert mc.to_lisp_atom(timedelta(seconds=30)) == "30.0"
    assert mc.to_lisp_atom(time(12, 0)) == '"12:00:00"'
    assert mc.to_lisp_atom(mc.Health.ERROR) == "'error"
    assert mc.to_lisp_atom(True) == "t"
    assert mc.to_lisp_atom(5) == "5"


def test_raw_splices_literal_lisp() -> None:
    node = meter(id=2, power=raw("(lambda () (+ 1000.0 (random 500)))"))
    assert ":power-w (lambda () (+ 1000.0 (random 500)))" in node.to_lisp()


def test_branching_is_a_successors_list() -> None:
    mg = Microgrid(
        id=1,
        topology=grid(
            id=1,
            successors=[
                meter(id=2, successors=[battery_inverter(id=3)]),
                meter(id=5, power=Power.from_watts(-2000)),
            ],
        ),
    )
    lisp = mg.to_lisp()
    assert "(make-battery-inverter :id 3)" in lisp
    assert "(make-meter :id 5 :power-w -2000.0)" in lisp


def test_public_constructors_exported() -> None:
    for name in (
        "grid",
        "meter",
        "battery_inverter",
        "solar_inverter",
        "battery",
        "ev_charger",
        "chp",
        "steam_boiler",
        "raw",
        "Microgrid",
    ):
        assert hasattr(mc, name), name


def test_builders_cover_every_server_arg() -> None:
    # One call per builder exercising the newly named parameters; each
    # keyword must land under the server's exact plist key and unit.
    g = mc.grid(
        id=1,
        rated_fuse_current=Current.from_amperes(63),
        stream_jitter=Percentage.from_percent(5),
    ).to_lisp()
    assert ":rated-fuse-current-a 63" in g
    assert ":stream-jitter-pct 5.0" in g

    inv = mc.battery_inverter(
        id=3,
        interval=timedelta(milliseconds=500),
        command_delay=timedelta(milliseconds=250),
        ramp_rate_w_per_s=1000.0,
        reactive_pf_limit=0.9,
        reactive_apparent=ApparentPower.from_volt_amperes(10000),
        reactive_command_delay=timedelta(milliseconds=100),
        reactive_ramp_rate_var_per_s=2000.0,
    ).to_lisp()
    assert ":interval-s 0.5" in inv
    assert ":command-delay-s 0.25" in inv
    assert ":ramp-rate-w-per-s 1000.0" in inv
    assert ":reactive-pf-limit 0.9" in inv
    assert ":reactive-apparent-va 10000.0" in inv
    assert ":reactive-command-delay-s 0.1" in inv
    assert ":reactive-ramp-rate-var-per-s 2000.0" in inv

    bat = mc.battery(
        id=4,
        soc_lower=Percentage.from_percent(10),
        soc_upper=Percentage.from_percent(90),
        soc_protect_margin=Percentage.from_percent(5),
        voltage=Voltage.from_volts(800),
    ).to_lisp()
    assert ":soc-lower-pct 10.0" in bat
    assert ":soc-upper-pct 90.0" in bat
    assert ":soc-protect-margin-pct 5.0" in bat
    assert ":voltage-v 800.0" in bat

    ev = mc.ev_charger(
        id=6,
        phases=1,
        idle=mc.EvIdle.FULL,
        command_delay=timedelta(milliseconds=200),
        ramp_rate_w_per_s=500.0,
    ).to_lisp()
    assert ":phases 1" in ev
    assert ":idle 'full" in ev
    assert ":command-delay-s 0.2" in ev
    assert ":capacity" not in ev, "the pack belongs to the car now"
    # Off by default: the charger trips and awaits a re-dispatch, so the
    # flag renders nothing at all rather than an explicit nil.
    assert ":resume-on-recovery" not in ev
    ev_resume = mc.ev_charger(id=6, resume_on_recovery=True).to_lisp()
    assert ":resume-on-recovery t" in ev_resume

    # The pack kwargs a charger used to take are refused by name here
    # rather than passed through **extra: the server takes them only to
    # warn and ignore them, which a Python caller would never see.
    for retired in (
        "capacity",
        "initial_soc",
        "soc_lower",
        "soc_upper",
        "soc_protect_margin",
    ):
        with pytest.raises(TypeError, match="the pack belongs to the car"):
            mc.ev_charger(id=6, **{retired: 1.0})

    import inspect

    # A named parameter, not something that only works via **extra.
    assert "resume_on_recovery" in inspect.signature(mc.ev_charger).parameters
    # make-chp takes no rated bounds; the builder no longer offers them.
    assert "rated" not in inspect.signature(mc.chp).parameters
    chp_lisp = mc.chp(id=7, stream_jitter=Percentage.from_percent(2)).to_lisp()
    assert ":stream-jitter-pct 2.0" in chp_lisp


def test_steam_boiler_renders_rated_and_physics_kwargs() -> None:
    c = mc.steam_boiler(
        id=7,
        rated=(Power.from_watts(0), Power.from_watts(100_000)),
        target_bar=6.0,
        max_bar=9.0,
        demand_kg_per_s=0.01,
    )
    text = c.to_lisp()
    assert "make-steam-boiler" in text
    assert ":rated-upper-w 100000.0" in text
    assert ":target-bar 6.0" in text
    assert ":max-bar 9.0" in text
    assert ":demand-kg-per-s 0.01" in text


def test_plug_ev_form_renders_every_override() -> None:
    assert plug_ev_form(
        6,
        "city",
        soc=Percentage.from_percent(20),
        target_soc=Percentage.from_percent(80),
        phases=1,
        max_current=Current.from_amperes(16),
        capacity=Energy.from_kilowatt_hours(45.0),
        taper_start=Percentage.from_percent(70),
        taper_floor=Percentage.from_percent(20),
    ) == (
        "(plug-ev 6 'city :soc-pct 20.0 :target-soc-pct 80.0"
        " :phases 1 :max-current-a 16.0 :capacity-wh 45000.0"
        " :taper-start-pct 70.0 :taper-floor-pct 20.0)"
    )
    # A bare preset emits no overrides at all, and the enum and its
    # string spelling render the same symbol.
    assert plug_ev_form(6, mc.EvPreset.CITY) == "(plug-ev 6 'city)"
    assert plug_ev_form(6, "city") == "(plug-ev 6 'city)"
    with pytest.raises(ValueError):
        plug_ev_form(6, "unicorn")


def test_plug_ev_form_refuses_bare_numbers() -> None:
    for name in ("soc", "target_soc", "taper_start", "taper_floor"):
        with pytest.raises(TypeError, match=name):
            plug_ev_form(6, "city", **{name: 20.0})
    with pytest.raises(TypeError, match="max_current"):
        plug_ev_form(6, "city", max_current=16.0)
    with pytest.raises(TypeError, match="capacity"):
        plug_ev_form(6, "city", capacity=45.0)


def test_typed_quantities_render_in_base_units() -> None:
    bat = battery(id=1, capacity=Energy.from_kilowatt_hours(50)).to_lisp()
    assert ":capacity-wh 50000.0" in bat
    inv = battery_inverter(id=2, command_delay=timedelta(milliseconds=200)).to_lisp()
    assert ":command-delay-s 0.2" in inv
    sun = mc.solar_inverter(id=3, sunlight=Percentage.from_percent(80)).to_lisp()
    assert ":sunlight-pct 80.0" in sun


def test_sunlight_takes_a_percentage_or_raw_lisp() -> None:
    assert (
        ":sunlight-pct 80.0"
        in mc.solar_inverter(id=1, sunlight=Percentage.from_percent(80)).to_lisp()
    )
    assert (
        ":sunlight-pct (lambda () 50.0)"
        in mc.solar_inverter(id=1, sunlight=raw("(lambda () 50.0)")).to_lisp()
    )
    with pytest.raises(TypeError, match="sunlight"):
        mc.solar_inverter(id=1, sunlight=80.0)  # type: ignore[arg-type]


def test_a_bare_number_where_a_quantity_belongs_raises() -> None:
    with pytest.raises(TypeError, match="capacity"):
        battery(capacity=50000.0)  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="initial_soc"):
        battery(initial_soc=60.0)  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="interval"):
        meter(interval=1.0)  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="stream_jitter"):
        grid(stream_jitter=5.0)  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="max_current"):
        plug_ev_form(1, "city", max_current=16.0)  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="rated"):
        battery_inverter(rated=(1.0, 2.0))  # type: ignore[arg-type]


_BUILDERS = (
    grid,
    meter,
    battery_inverter,
    mc.solar_inverter,
    battery,
    mc.ev_charger,
    mc.chp,
    mc.steam_boiler,
)


@pytest.mark.parametrize(
    ("builder", "old", "new"),
    [
        (builder, old, new)
        for builder in _BUILDERS
        for old, new in (
            ("stream_jitter_pct", "stream_jitter"),
            ("reactive_apparent_va", "reactive_apparent"),
            ("ramp_rate", "ramp_rate_w_per_s"),
            ("reactive_ramp_rate", "reactive_ramp_rate_var_per_s"),
            ("demand_kg_h", "demand_kg_per_s"),
        )
    ],
)
def test_a_retired_argument_name_raises(builder: Any, old: str, new: str) -> None:
    # The replacement is named only to a builder that has it.
    if new in inspect.signature(builder).parameters:
        message = f"^{old} was renamed; use {new}$"
    else:
        message = f"^unexpected argument {old}$"
    with pytest.raises(TypeError, match=message):
        builder(id=1, **{old: 1.0})


@pytest.mark.parametrize(
    ("builder", "spelled", "typed"),
    [
        (builder, spelled, typed)
        for builder in _BUILDERS
        for spelled, typed in (
            ("power_w", "power"),
            ("capacity_wh", "capacity"),
            ("interval_s", "interval"),
            ("command_delay_s", "command_delay"),
            ("initial_soc_pct", "initial_soc"),
            ("sunlight_pct", "sunlight"),
            ("voltage_v", "voltage"),
            ("rated_lower_w", "rated"),
            ("rated_upper_w", "rated"),
        )
    ],
)
def test_a_unit_spelled_quantity_argument_raises(
    builder: Any, spelled: str, typed: str
) -> None:
    # The plist key spelled as an argument would skip the type check. The
    # typed argument is named only to a builder that has it.
    if typed in inspect.signature(builder).parameters:
        message = f"^no argument {spelled}; use {typed}, a typed value$"
    else:
        message = f"^unexpected argument {spelled}$"
    with pytest.raises(TypeError, match=message):
        builder(id=1, **{spelled: 1.0})


def test_a_steam_boiler_writes_its_pacing_arguments() -> None:
    text = mc.steam_boiler(
        id=1,
        interval=timedelta(seconds=2),
        command_delay=timedelta(milliseconds=500),
        ramp_rate_w_per_s=100.0,
    ).to_lisp()
    assert ":interval-s 2.0 :command-delay-s 0.5 :ramp-rate-w-per-s 100.0" in text


def test_a_replacement_passed_through_extra_is_not_named() -> None:
    # The meter has no ramp rate; an `**extra` spelling of the new name
    # does not make it one the meter takes.
    with pytest.raises(TypeError, match="^unexpected argument ramp_rate$"):
        meter(id=1, ramp_rate=1.0, ramp_rate_w_per_s=1.0)


_OLD_KEYWORDS = {
    ":rated-fuse-current",
    ":rated-lower",
    ":rated-upper",
    ":interval",
    ":power",
    ":reactive-power",
    ":capacity",
    ":capacity-kwh",
    ":initial-soc",
    ":soc-lower",
    ":soc-upper",
    ":soc-protect-margin",
    ":voltage",
    ":command-delay-ms",
    ":device-delay-ms",
    ":reactive-command-delay-ms",
    ":ramp-rate",
    ":reactive-ramp-rate",
    ":sunlight%",
    ":demand",
    ":soc",
    ":target-soc",
    ":taper-start",
    ":taper-floor",
    ":component",
}


def test_every_builder_emits_only_the_new_keywords() -> None:
    pct = Percentage.from_percent(10)
    delay = timedelta(milliseconds=300)
    rated = (Power.from_kilowatts(-1), Power.from_kilowatts(1))
    forms = [
        grid(
            id=1,
            rated=rated,
            rated_fuse_current=Current.from_amperes(63),
            stream_jitter=pct,
        ),
        meter(
            id=2,
            power=Power.from_watts(1),
            reactive_power=ReactivePower.from_kilo_volt_amperes_reactive(1),
            interval=delay,
            stream_jitter=pct,
        ),
        battery_inverter(
            id=3,
            rated=rated,
            interval=delay,
            command_delay=delay,
            ramp_rate_w_per_s=1.0,
            reactive_pf_limit=0.5,
            reactive_apparent=ApparentPower.from_volt_amperes(1),
            reactive_command_delay=delay,
            reactive_ramp_rate_var_per_s=1.0,
            stream_jitter=pct,
        ),
        mc.solar_inverter(
            id=4,
            rated=rated,
            sunlight=pct,
            interval=delay,
            command_delay=delay,
            ramp_rate_w_per_s=1.0,
            reactive_apparent=ApparentPower.from_volt_amperes(1),
            reactive_command_delay=delay,
            reactive_ramp_rate_var_per_s=1.0,
            stream_jitter=pct,
        ),
        battery(
            id=5,
            capacity=Energy.from_watt_hours(1),
            initial_soc=pct,
            rated=rated,
            soc_lower=pct,
            soc_upper=pct,
            soc_protect_margin=pct,
            voltage=Voltage.from_volts(1),
            interval=delay,
            stream_jitter=pct,
        ),
        mc.ev_charger(
            id=6,
            rated=rated,
            interval=delay,
            command_delay=delay,
            ramp_rate_w_per_s=1.0,
            stream_jitter=pct,
        ),
        mc.chp(id=7, stream_jitter=pct),
        mc.steam_boiler(id=8, rated=rated, demand_kg_per_s=0.1, stream_jitter=pct),
    ]
    text = " ".join(f.to_lisp() for f in forms)
    text += plug_ev_form(
        6,
        "city",
        soc=pct,
        target_soc=pct,
        phases=1,
        max_current=Current.from_amperes(16),
        capacity=Energy.from_watt_hours(1),
        taper_start=pct,
        taper_floor=pct,
    )
    text += _check_text()
    used = set(re.findall(r"(?<![\w-]):[\w%-]+", text))
    assert used, text
    assert used.isdisjoint(_OLD_KEYWORDS), used & _OLD_KEYWORDS


def _check_text() -> str:
    from macrocosim.enums import Metric
    from macrocosim.matchers import at_most
    from macrocosim.scenarios import Scenario

    return (
        Scenario("s", length=timedelta(seconds=10))
        .check_metric(
            timedelta(seconds=1),
            component_id=2,
            metric=Metric.ENERGY,
            matcher=at_most(Power.from_watts(1)),
        )
        .to_lisp()
    )
