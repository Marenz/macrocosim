# macrocosim

A microgrid simulator for testing downstream control apps. Components
(grid, meter, battery, inverters, EV charger, CHP, steam boiler — a
controllable gas/electric hybrid) are Rust types behind a single
`SimulatedComponent` trait; the topology + animation script is Lisp
via [`tulisp`](https://github.com/shsms/tulisp).

The simulator exposes three surfaces:

- **gRPC** — Frequenz `Microgrid` v1alpha18 API. One binary can
  serve many microgrids; the first defaults to `[::1]:8800` and
  subsequent ones step by ten (`:8810`, `:8820`, …), or pin an
  explicit port with `:grpc-port` on `(make-microgrid …)`.
  Downstream apps written against the production API talk to
  macrocosim the same way they'd talk to a real microgrid.
  Two enterprise-wide services ride alongside on their own
  sockets: `PlatformAssets` (`[::1]:9900`) and the
  `MicrogridDispatchService` store-and-serve dispatch API
  (`[::1]:8900`), each keyed by `microgrid_id` per request.
- **Web UI** (`http://127.0.0.1:8801`) — multi-microgrid SPA.
  Each microgrid gets a topology canvas (edit with undo / redo,
  pick a layout, align nodes), a per-component chart dashboard,
  and a *Formulas* tab. The Formulas tab shows every generated
  formula and the engine's reason for each part. The UI can
  also import a microgrid API site export as a real simulated
  microgrid, and run scenarios. Raw JS / HTML / CSS embedded
  into the binary via `rust-embed`; no build step.
- **macroctl** — clap-based client that drives both surfaces from the
  shell.

## Build & run

```sh
cargo build
cargo run --bin macrocosim examples/berlin-demo.lisp
```

The binary takes zero or more Lisp scripts. Each script is a
self-contained world — `examples/berlin-demo.lisp` wires a demo
topology, animates its AC environment, and registers seven starter
scenarios; saving the file hot-reloads the world. With no scripts
the engine boots bare: UI up, no microgrids, and you load a script
on demand from the Microgrids tab or the REPL (the `repl` pill on a
microgrid's Topology view, or a backtick anywhere, opens it):

```lisp
(load "examples/berlin-demo.lisp")
```

Relative paths (and all persistent state: managed microgrid files
under `microgrids/`, `enterprise.lisp`, `snapshots/`) anchor to
`--state-dir`, defaulting to the directory the server was started
from. See `sim/defaults.lisp` for the per-category default knobs and
`sim/common.lisp` for the runtime helpers — both are embedded into
the binary, so a script never needs to load them.

The proto roots ([frequenz-api-microgrid](https://github.com/frequenz-floss/frequenz-api-microgrid),
frequenz-api-assets, frequenz-api-dispatch) are vendored as git
submodules under `submodules/` — run `git submodule update --init --recursive`
once after cloning. `MACROCOSIM_PROTO_ROOT` overrides the microgrid
proto root for downstream packagers with a private mirror.

## Scenarios

A scenario is a Lisp script that drives the simulator through stress
events (load spikes, cloud cover, random outages, silent components)
while a Rust reporter records peak / charge / discharge / SoC stats
and per-15-minute averages. See [`scenarios/README.md`](scenarios/README.md)
for the framework, [`examples/scenario-driving.lisp`](examples/scenario-driving.lisp)
for a runnable 30-minute sample.

```sh
macroctl scenario start "demo"
macroctl scenario load examples/scenario-driving.lisp
macroctl scenario report
macroctl scenario events --since 0 --limit 20
macroctl scenario stop
```

The Report panel in the web UI polls the same endpoints as `macroctl
scenario report`.

## macroctl

```sh
macroctl info
macroctl tree
macroctl list --category battery
macroctl connections --from 4                                  # filter graph edges
macroctl stream 1001 --samples 5
macroctl set-power 1001 -5000 --lifetime 30                    # negative = discharge
macroctl augment-bounds 1001 --lower -1000 --upper 5000        # TTL-limited bounds
macroctl pool battery                                          # loopback BatteryPool snapshot
macroctl scenario report                                       # journal report / CI gate
macroctl scenario list                                         # registered scenarios
macroctl scenario run cloud-fade --wait --assert              # run one live + gate
macroctl snapshot save before-test                             # freeze the mg's managed file
macroctl dashboard --tail                                      # one-line/sec pulse bar
macroctl dispatch list                                         # dispatch API CRUD
macroctl dispatch create <type> battery --duration 3600
macroctl --microgrid-id 2 info                                 # pick another microgrid
```

`--microgrid-id` selects the microgrid for the gRPC commands,
`dispatch`, `snapshot`, `pool`, `dashboard` and the scenario readouts
(`summary`, `report`, `events`, `run --wait`); the default is the
lowest id. `scenario list` / `load` are site-wide. `scenario start` /
`stop` and a run's start and stop reach every microgrid's journal;
`scenario event` and a scenario's checks and recordings land on the
lowest microgrid, so `--assert` works only on that one. `--addr`
names the gRPC server directly; with `--microgrid-id` the lookup
still runs and the two must agree. `--ui-addr` (default
`http://127.0.0.1:8801`) points at the UI server, which the
HTTP-driven verbs (`scenario*`, `snapshot`, `dashboard`, `pool`) talk
to. The gRPC commands ask it for the microgrid's address unless
`--addr` alone is given, and `dispatch` asks it for the lowest
microgrid id when `--microgrid-id` is not given. `--json` swaps any
human table for the raw JSON.

The `scenario` subcommand covers both the ad-hoc journal verbs
(start / stop / event / load / report / events / summary) and the
registered scenarios from `(define-scenario …)`: `list` them and
`run NAME` — live (`--wait`) or headless deterministic (`--stepped`),
with `--assert` to gate CI.

## Configuration knobs

- **Main / point-of-common-coupling meter** — derived from the
  topology, not flagged: it's the grid connection point's sole child
  when that child is a meter. The scenario reporter tracks its peak.
- **`(make-meter :power-w N | (lambda () …) | 'symbol)`** — drive the
  meter's published power from a constant, a lambda, or a global
  symbol. Same on solar inverters via `:sunlight%`.
- **`(set-meter-power id N | (lambda () …) | 'symbol)`** — same
  polymorphism imperatively, for `(every …)` callbacks or scenario
  scripts. Numeric values collapse any prior dynamic source.

## Architecture in one paragraph

A `MicrogridSite` owns one microgrid's component registry, physics
tick loop, telemetry-history rings, and scenario journal; an
enterprise `microgrids` registry keys those sites by id so one
binary can serve many at once. Lisp's only jobs are wiring topology
(`(make-grid)`, `(make-meter)`, … inside the `:topology` lambda of
`(make-microgrid …)`) and animating the environment (`(every …)`,
`(run-with-timer …)`, `(set-meter-power)`, etc.) — every component's
tick / ramp / SoC stays in Rust. Inverter and battery share
only an electrical coupling: the battery's BMS clamps DC ingress,
the inverter publishes the measured aggregate, and a server-side
gateway intersects bounds for setpoint validation.

## More

- [`docs/e2e-testing.md`](docs/e2e-testing.md) — drive macrocosim from a
  downstream app's CI for end-to-end integration tests.
- [`AGENTS.md`](AGENTS.md) — developer notes for this repo.
- [`todo.org`](todo.org) — roadmap + open design questions.
- [`scenarios/README.md`](scenarios/README.md) — scenario framework
  reference.

## License

The simulator (this crate and the `macrocosim` / `macroctl` binaries) is
[GPL-3.0-only](LICENSE). The Python client under [`python/`](python/) is
[MIT](python/LICENSE-MIT); its wheel bundles the GPL binaries and ships their
license alongside.

---

This project was previously called `switchyard`.
