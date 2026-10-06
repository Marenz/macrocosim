# macrocosim (Python)

Python integration-testing client for the
[macrocosim](https://github.com/shsms/macrocosim) microgrid simulator.

Build a topology, launch the simulator, drive its environment, inject
faults, and assert on the resulting grid state — from inside `pytest`,
talking to it the same way a downstream control app would (the gRPC
Microgrid API + the HTTP `/api` control plane).

```python
from datetime import timedelta
from frequenz.quantities import Energy, Percentage, Power
import macrocosim as mc

async def test_limiter_holds_import_cap():
    bat = mc.battery(id=4, capacity=Energy.from_kilowatt_hours(92),
                     initial_soc=Percentage.from_percent(50))
    inv = mc.battery_inverter(id=3, successors=[bat],
        rated=(Power.from_kilowatts(-5), Power.from_kilowatts(5)))
    mg  = mc.Microgrid(id=1,
        topology=mc.grid(id=1, successors=[mc.meter(id=2, successors=[inv])]))

    with mc.launch(mg) as site:
        site.component(inv).status(health=mc.Health.ERROR)            # fault injection
        await site.component(inv).expect.active_power(
            approx=Power.from_watts(0), tol=Power.from_watts(100))
        await site.component(bat).expect.soc(
            within=(Percentage.from_percent(45), Percentage.from_percent(55)))
```

## Install

Released as a **platform wheel** that bundles the `macrocosim` + `macroctl`
binaries (built from the Rust crate via maturin), so a downstream install
just works — no separate binary to fetch:

```sh
pip install 'macrocosim[grpc]'      # or: uv add 'macrocosim[grpc]'
```

`launch()` finds the bundled binaries automatically (in the interpreter's
scripts directory, even off PATH). Override with `MACROCOSIM_BIN` /
`MACROCTL_BIN`, or `bin=` on `launch`, to point at a local build.

## Development

This package is [uv](https://docs.astral.sh/uv/)-native; the build backend
is maturin, so `uv sync` compiles the binaries from the repo's Rust crate
(needs the Rust toolchain + `git submodule update --init`).

```sh
uv sync                       # venv from uv.lock; builds the binaries
uv run pytest                 # tests; ruff / ty also via `uv run`
```

Point tests at a fast local `cargo` build instead of the wheel's binary:

```sh
MACROCOSIM_BIN=../target/debug/macrocosim uv run pytest
```

## The surface

**Build** a topology — the spec *is* the graph (nested `successors`),
rendering 1:1 to macrocosim's `(make-*)` Lisp:

```python
from frequenz.quantities import Energy, Percentage, Power

P = Power.from_kilowatts
mg = mc.Microgrid(id=1, topology=mc.grid(id=1, successors=[
    mc.meter(id=2, successors=[
        mc.battery_inverter(id=3, rated=(P(-5), P(5)), successors=[
            mc.battery(id=4, capacity=Energy.from_kilowatt_hours(100),
                       initial_soc=Percentage.from_percent(50))]),
        mc.solar_inverter(id=5, sunlight=Percentage.from_percent(80)),
        mc.meter(id=6, power=Power.from_watts(1000))])]))
```

Constructors: `grid`, `meter`, `battery_inverter`, `solar_inverter`,
`battery`, `ev_charger`, `chp`, `steam_boiler`. Kwargs mirror the plist keys
(`snake_case` → `:kebab-case`): `rated=(lo, hi)` `Power` bounds, `capacity`
an `Energy`, `initial_soc` / `sunlight` a `Percentage`; `mc.raw("(lambda () …)")`
splices Lisp. `ev_charger` takes no pack: `phases` and `idle` (`mc.EvIdle`)
say what it offers, and `Site.plug_ev` plugs a car after launch.

Typed throughout — no bare numbers or unit strings. Knobs take enums
(`mc.Health`, `mc.CommandMode`, `mc.TelemetryMode`, scenario `mc.Metric`,
`mc.Schedule`); quantities are
[`frequenz-quantities`](https://pypi.org/project/frequenz-quantities/)
(`Power`, `Energy`, `Percentage`, `Frequency`), imported from it directly;
times are `datetime` (`timedelta`, or a `datetime.time` for an absolute scenario clock).
`mc.to_lisp_atom(v)` shows the Lisp literal any of them emits.
Or use an existing config: `mc.launch("topology.lisp")`.

**Launch** and get a `Site` (a context manager; tears the process down):

```python
with mc.launch(mg) as site:
    site.grpc          # first microgrid's gRPC address ("host:port")
    site.grpc_url      # ... as a "grpc://host:port" URL a client connects with
    site.eval("(...)") # raw Lisp escape hatch
```

**Read** — component telemetry over gRPC (what the app sees), aggregates
over the graph-derived formulas:

```python
site.active_power(3)      # Power | None       (component, gRPC)
site.soc(4)               # Percentage | None  (battery SoC, gRPC)
site.grid_power()         # Power | None       (/ pv_power() / consumer_power() / …)
```

`soc` is a battery's. A charger streams no SoC over gRPC — the pack
belongs to the car, which is the simulator's business and not the
API's — so read a plugged car's charge with `ev_info` below.

**EV chargers** — plug state, outside gRPC: plug/unplug go through Lisp,
`ev_info` reads a JSON route:

```python
site.plug_ev(1, mc.EvPreset.CITY, soc=40)  # None  (plug a preset car in)
site.unplug_ev(1)         # bool               (False if already empty)
site.ev_info(1)           # dict               ({"plugged": False} or the fields,
                          #                      plus `presets` either way)
```

**Energy** — the simulator integrates each power aggregate into a cumulative
energy stream (server-side), so you can judge what an app *did* over a run:

```python
site.grid_energy()        # Energy | None  (cumulative, import positive)
site.battery_energy()     # Energy | None  (/ consumer_energy() / pv_energy())
```

Assert on them through `expect` (a one-shot check — energy accumulates, so it
isn't a settling value to poll):

```python
await site.expect.grid_energy(max=Energy.from_kilowatt_hours(15))   # held import down
await site.expect.battery_energy(approx=Energy.from_kilowatt_hours(-8),
                                 tol=Energy.from_kilowatt_hours(1))  # discharge total
```

Per-component energy is a first-class metric too, assertable from Lisp and the
scenario framework: `(check "15m" :component-id 2 :metric 'energy :max 15000.0)` or
`metric=mc.Metric.ENERGY` on a Python `Scenario.check`.

**Mutate** — reach a component with `site[id]` (or `site.component(id)`), then
act by *intent*:

```python
inv = site[3]
inv.command(active_power=Power.from_kilowatts(2),      # app command (gRPC gateway)
            lifetime=timedelta(seconds=30))
inv.command(bounds=(Power.from_kilowatts(-1),
                    Power.from_kilowatts(1)))           # narrow the envelope
inv.status(health=mc.Health.ERROR)                       # inject a fault
site[6].drive(power=Power.from_megawatts(2))            # drive the environment
site[5].drive(sunlight=Percentage.from_percent(30))
```

`command` goes through the real gRPC gateway, so an out-of-envelope value
raises `mc.SetpointRejected` (the production behaviour under test); `status` /
`drive` are test-side stimuli.

**Assert** — settle-aware `expect`, on a component (`site[id].expect`) or a
microgrid aggregate (`site.expect`). The settle-aware assertions are `async`
(they `await` between polls, so an app under test on the same event loop keeps
running); the cumulative-energy ones are one-shot but `async` too, for a
uniform surface:

```python
await site[3].expect.active_power(
    approx=Power.from_kilowatts(2), tol=Power.from_watts(300),
    timeout=timedelta(seconds=15))
await site.expect.grid_power(
    max=Power.from_megawatts(1), for_=timedelta(seconds=30))
await site[4].expect.soc(
    within=(Percentage.from_percent(45), Percentage.from_percent(55)))
```

`await expect.<metric>(…)` polls until the matcher holds; pass `for_=` to
require it on every sample across a duration instead. Matchers: `approx`+`tol`,
`within`, `max`, `min`.

**Async core (v2): signals** — `macrocosim.aio` is the async-native
core: every read, write, and wait is a coroutine on your event loop (no
background threads). Its unit is the *signal*: once `aio.launch` binds
the topology, the builder objects are the live handles, and every
observable quantity is an object with up to three verbs — `read`
(returns the plain quantity; raises `NoSample`), `expect` (takes one
typed matcher), and `set` where the simulator allows it. Capability
lives in the type:

```python
load = mc.meter(id=5, power=Power.zero())
bat = mc.battery(id=4, capacity=Energy.from_kilowatt_hours(100),
                 initial_soc=Percentage.from_percent(60))

async with mc.aio.launch(mg) as site:
    await load.power.set(Power.from_kilowatts(20))       # drive the world
    await site.grid_power.expect(mc.at_most(Power.from_kilowatts(13)))
    await bat.soc.set(Percentage.from_percent(11))       # teleport state
    await bat.soc.expect(mc.between(Percentage.from_percent(10),
                                    Percentage.from_percent(12)))
    stored = await bat.stored_energy.read()              # state (SoC×capacity)
    await site.battery_energy.expect(                    # flow (∫ battery_power)
        mc.at_most(Energy.from_watt_hours(-1)))
    await inv.health.set(mc.Health.ERROR)                # fault injection
```

Matchers: `near(x, tol=…)`, `between(lo, hi)`, `at_most(x)`,
`at_least(x)` — one per expect, typed by the signal's quantity. An
inverter's `power` has no `.set` (commanding it is `site[inv].command()`
through the real gateway), and a cumulative signal's `expect` has no
`hold_for` — the distinctions are in the types, not runtime errors.
Every `*_energy` site aggregate is the integral of its `*_power`; energy
*stored* in a battery is `bat.stored_energy`. Stimuli go over typed JSON
control endpoints; rejections raise `ControlRejected`. See
`../docs/python-api-redesign.org` for the design.

**Scenarios** — author in Python, or run a registered Lisp scenario:

```python
scn = mc.Scenario("cloud-fade", length=timedelta(minutes=4))
scn.at(timedelta(seconds=30), pv.sunlight, Percentage.from_percent(20))
scn.check(timedelta(seconds=110), inv.power,
          mc.near(Power.from_megawatts(1.5), tol=Power.from_kilowatts(300)))
site.define_scenario(scn).run(wait=True).assert_passed()

# deterministic, serverless gate (no app under test):
mc.run_scenario_stepped([mg, scn], "cloud-fade")
```

**pytest** — the plugin auto-loads; provide a `macrocosim_config` fixture:

```python
@pytest.fixture
def macrocosim_config():
    return mg

async def test_grid_holds(macrocosim):
    await macrocosim.expect.grid_power(
        approx=Power.from_kilowatts(7), tol=Power.from_watts(500))

@pytest.mark.macrocosim_scenario("cloud-fade")   # runs and gates after a
                                                # passing test body; a failed
                                                # check is a teardown error
def test_scenario(macrocosim): ...
```

The `expect` assertions are `async`, so awaiting them needs `pytest-asyncio`
(installed with the `grpc` extra) and `asyncio_mode = "auto"` in your pytest
config — otherwise an `async def` test is collected but never awaited, and the
assertion silently never runs.

## Status

Early but functional end to end — see `todo.org` §Y in the macrocosim repo
for the design and roadmap. Building, launching, reading and mutating are
synchronous; the settle-aware `expect` assertions are `async` (so they compose
with an app under test on the same event loop). Runnable `examples/` cover each
piece; `examples/pytest_demo/` is a live suite.

## License

The Python package is [MIT](LICENSE-MIT). The wheel also bundles the
`macrocosim` and `macroctl` binaries, which are
[GPL-3.0-only](LICENSE-GPL-3.0); their source is the
[macrocosim repository](https://github.com/shsms/macrocosim) at the
release's git tag: `v0.1.0` for wheel version 0.1.0, and for a pre-release
the semver form of the PEP 440 version, so `v0.1.0-alpha.1` for 0.1.0a1.
