"""Y0 walking skeleton, now with the Python topology builder.

The thinnest end-to-end path: build a topology in Python → launch → read a
formula → assert. Run against a built binary:

    MACROCOSIM_BIN=../target/debug/macrocosim python examples/walking_skeleton.py

An existing ``.lisp`` file works too — ``mc.launch("skeleton.lisp")`` or
``mc.launch(mc.Microgrid.from_lisp_file("skeleton.lisp"))``.
"""

from __future__ import annotations

from datetime import timedelta

from frequenz.quantities import Power

import macrocosim as mc

# The spec IS the graph: the meter is the grid's sole child, so it's the
# derived main/PCC meter and grid_power tracks its 7 kW.
TOPOLOGY = mc.Microgrid(
    id=1,
    topology=mc.grid(id=1, successors=[mc.meter(id=2, power=Power.from_watts(7000))]),
)


def main() -> None:
    with mc.launch(TOPOLOGY) as site:
        mg = next(iter(site.microgrids.values()))
        print(f"launched: ui={site.ui} grpc={mg.grpc_addr} (mg {mg.id} {mg.name!r})")

        target = Power.from_watts(7000)
        tol = Power.from_watts(700)
        value = site.read_until(
            site.grid_power,
            lambda v: v is not None and abs(v - target) < tol,
            timeout=timedelta(seconds=15),
        )
        assert value is not None and abs(value - target) < tol, (
            f"grid_power did not settle near 7 kW; last read: {value}"
        )
        print(f"OK  grid_power = {value}")


if __name__ == "__main__":
    main()
