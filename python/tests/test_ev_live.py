"""Plug a car, watch it charge past the 6 A floor, unplug it.

Skips when no ``macrocosim`` binary is available.
"""

from __future__ import annotations

import os
import time
from collections.abc import Callable

import pytest
from frequenz.quantities import Percentage, Power

import macrocosim as mc
from macrocosim._process import which_binary

if not (os.environ.get("MACROCOSIM_BIN") or which_binary("macrocosim")):
    pytest.skip("no macrocosim binary available", allow_module_level=True)


def _wait(pred: Callable[[], object], limit_s: float = 15.0, every: float = 0.2):
    deadline = time.monotonic() + limit_s
    while True:
        v = pred()
        if v:
            return v
        if time.monotonic() >= deadline:
            raise AssertionError("timed out")
        time.sleep(every)


def test_plug_charge_unplug() -> None:
    charger = mc.ev_charger(id=6, rated=(Power.zero(), Power.from_kilowatts(22)))
    mg = mc.Microgrid(
        id=1,
        topology=mc.grid(id=1, successors=[mc.meter(id=2, successors=[charger])]),
    )
    with mc.launch(mg) as site:
        assert site.ev_info(6)["plugged"] is False

        site.plug_ev(6, mc.EvPreset.CITY, soc=Percentage.from_percent(20))
        info = site.ev_info(6)
        assert info["plugged"] is True and info["preset"] == "city"
        assert info["phases"] == 1

        # Paused idle: nothing flows until a command stands.
        site.eval("(set-active-power 6 22000 :lifetime-s 60)")

        def charging_watts() -> float | None:
            # Returns the crossing sample itself (not just True) so the
            # cap assertion below has a real number to test.
            watts = (site.active_power(6) or Power.zero()).as_watts()
            return watts if watts > 7000 else None

        p = _wait(charging_watts)
        assert p < 7500, "a 1-phase 32 A car tops out near 7.4 kW"
        assert site.ev_info(6)["state"] == "charging"

        # Under the 6 A floor the charger pauses.
        site.eval("(set-active-power 6 4000 :lifetime-s 60)")
        _wait(lambda: site.ev_info(6)["state"] == "paused")

        assert site.unplug_ev(6) is True
        assert site.ev_info(6)["plugged"] is False
        _wait(lambda: (site.active_power(6) or Power.zero()).as_watts() < 1)
