# End-to-end testing your app against macrocosim in CI

Macrocosim is a high-fidelity stand-in for a real microgrid: it speaks the
production Frequenz gRPC API (`Microgrid`, `PlatformAssets`,
`MicrogridDispatchService`), runs real component physics, and drives scripted
scenarios with built-in pass/fail assertions. That makes it a drop-in
environment for a downstream control app's integration tests — your app talks to
macrocosim exactly as it would to a real deployment.

Macrocosim runs as a **separate process**; your app connects over gRPC. There's
nothing to link or vendor — build the `macrocosim` + `macroctl` binaries from this
repo (a published wheel / container image is planned, see todo.org §D5) and
drive them from your CI.

## Two ways to test

### 1. Deterministic, serverless gate (no app under test)

If the thing you're asserting is the *simulation's* own behaviour — a scenario's
`(check …)` assertions — run it headless on a stepped clock. Deterministic,
faster than real time, no server, no app:

```sh
macroctl scenario run my-scenario \
  --stepped --config graph.lisp --assert
#  --until <secs>  override the scenario's :length
#  --step <ms>     clock step (default 100)
```

This boots a headless simulator in-process, runs `my-scenario` to its `:length`,
and exits non-zero if any `(check …)` failed. Reproducible with a `:seed`. Good
for testing scenario fixtures themselves, or sim-side invariants.

### 2. Live end-to-end with your app

The real integration test: macrocosim drives the environment (PV, load, faults)
while *your app* reacts over gRPC, and a scenario asserts the resulting grid
state.

```sh
# 1. boot the simulator; ephemeral ports keep parallel CI jobs from clashing,
#    and the endpoints file is the readiness signal.
macrocosim graph.lisp --ephemeral-ports --emit-endpoints=endpoints.json &
SIM=$!
# wait until bound; bail out if the simulator died at boot instead
# of hanging the job until CI's global timeout.
until [ -s endpoints.json ]; do kill -0 $SIM 2>/dev/null || exit 1; sleep 0.1; done

GRPC=$(jq -r '.microgrids[0].grpc' endpoints.json)   # e.g. [::1]:41979
UI=$(jq -r '.ui' endpoints.json)                     # e.g. 127.0.0.1:33565

# 2. boot YOUR app against the simulator's gRPC address.
my-control-app --microgrid "grpc://$GRPC" &
APP=$!

# 3. run a scenario live, block until it finishes, gate on its checks.
macroctl --ui-addr "http://$UI" scenario run my-scenario --wait --assert
RC=$?

kill $APP $SIM
exit $RC
```

`scenario run … --wait` starts the scenario, blocks until it finishes (its
`:length`, or `--until <secs>`), stops it, and with `--assert` exits non-zero on
any failed `(check …)`. On failure, upload your app's logs and any recorded CSVs
(`(scenario-record-csv …)`) as CI artifacts.

`--emit-endpoints` prints one JSON line of the resolved addresses — to a file
(as above) or stdout if given no path:

```json
{"ui":"127.0.0.1:33565","microgrids":[{"id":9,"name":"demo","grpc":"[::1]:41979"}],"assets":"[::1]:33881","dispatch":"[::1]:43253"}
```

## Structuring fixtures

A test case is a **(graph, scenario, app-config)** triple. Keep these fixtures in
*your* repo — macrocosim is a generic engine that loads them.

- **One file per graph.** Define the topology once, then every scenario that
  exercises it as a `(define-scenario …)` in the same file:

  ```lisp
  (make-microgrid :id 9 :grpc-port 8800 :topology (lambda () … ))
  (define-scenario :name "cloud-fade"  :schedule 'relative :length "4min" …)
  (define-scenario :name "pv-dropout"  …)
  ```

  No `(load "sim/…")` needed — the scenario DSL vocabulary (`define-scenario`,
  `at` / `check` / `controller` / `drive-meter` / `drive-solar` / `timeline`,
  `set-*`) is embedded in the binary.

- **Parallelise at the CI-job level.** Run one isolated `macrocosim` + app pair
  per `(graph, scenario)` — a CI matrix. With `--ephemeral-ports` the jobs don't
  collide. This gives true isolation, independent retry, and parallelism over
  the (real-time) live runs.

- **Sequential fallback.** If spinning a process pair per case is too heavy, run
  a graph's scenarios in sequence against one boot: `scenario run A --wait
  --assert; scenario run B --wait --assert; …` — each `scenario-start` resets the
  journal. Simpler infra; wall-clock is the sum of the runs.

- **Multiple microgrids in one file** is for a single test case that is
  *genuinely* multi-microgrid (a fleet). It is **not** the way to parallelise
  independent cases — a scenario is enterprise-wide (one at a time) and the
  report reads one microgrid, so use CI-job parallelism for independent cases.

## Determinism

The headless stepped gate (mode 1) is bit-reproducible with a `:seed`. The live
gate (mode 2) is **real-time** by nature — your app is a black box on its own
clock — so write `(check …)` assertions against **settled-state invariants with
tolerance** (e.g. "import held at/under the cap ± slack" a comfortable margin
after the perturbation), not exact transient values. Configure components with
zero command-delay so steady state is reached quickly.

## Reference

- `macrocosim <config>` — `--ui-port N` · `--ephemeral-ports` ·
  `--emit-endpoints[=PATH]`.
- `macroctl scenario run NAME` — `--stepped --config X [--until S] [--step MS]`
  (headless) · `--wait [--until S]` (live) · `--assert` (gate).
- `macroctl scenario report [--assert]`, `macroctl scenario list`,
  `macroctl --microgrid-id N --ui-addr …` (the global
  `--microgrid-id` selects the microgrid for the readouts, default
  the lowest id; `--assert` works only on the lowest microgrid,
  where a scenario's checks land; `--addr` optionally overrides the
  gRPC address).
- [`scenarios/DESIGN.md`](../scenarios/DESIGN.md) — the scenario model.
