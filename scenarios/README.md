# Scenarios

A scenario is a Lisp file that drives macrocosim through a stress
test — sudden load spikes, cloud cover, battery outages, silent
components — while a Rust **reporter** records metrics and a
**journal** records named events. At any time you can ask the
running simulator what's happened so far via
`GET /api/mg/{mg}/scenario/report` or the **Report** panel in the UI.

The framework is two layers:

- **Driver** (the script you write) — schedules events using
  `(every …)` and `(run-with-timer …)` plus a small set of
  scenario-specific defuns. See `examples/scenario-driving.lisp`.
- **Reporter** (Rust observer, always running) — accumulates peak
  grid power, battery charge / discharge integrals, PV
  produced energy, SoC stats, and 15-minute window averages.
  Resets on `scenario-start`; freezes on `scenario-stop`.

## Loading a scenario

There's no auto-discovery. Load explicitly from the REPL —

```lisp
(load "examples/scenario-driving.lisp")
```

— or put the forms in the world script itself so they run at load
time (`examples/berlin-demo.lisp` registers its seven
`define-scenario`s that way). `(load …)` resolves relative to the
server's state dir (`--state-dir`, default: the directory the
server was started from).

## Lifecycle defuns

| Defun                              | Purpose                                          |
|------------------------------------|--------------------------------------------------|
| `(scenario-start NAME)`            | begin a run — clears the journal + reporters; tears down a run still in progress first (see [Teardown](#teardown)) |
| `(scenario-stop)`                  | end the run — freezes elapsed + metrics + CSV, cancels the run's own timers, restores every driven knob (see [Teardown](#teardown)) |
| `(scenario-event KIND PAYLOAD)`    | append a journaled event                        |
| `(scenario-elapsed)`               | wall-clock seconds since start (frozen on stop) |
| `(scenario-end-after-s SECONDS)`   | schedule `(scenario-stop)` after SECONDS        |
| `(scenario-record-csv DIR)`        | start writing one CSV per component to DIR     |
| `(scenario-stop-csv)`              | close all CSV sinks (also implicit on stop)    |

`KIND` accepts a symbol or a string. `PAYLOAD` accepts any value
— it renders through Display when stored.

## Driving the environment

These setters work outside scenarios too — they're the same
animation knobs `examples/berlin-demo.lisp` uses for its built-in
load and cloud curves — but inside a scenario they're how a script
exercises the simulator:

| Defun                                  | Effect                                                   |
|----------------------------------------|----------------------------------------------------------|
| `(set-meter-power ID VAL)`             | drive a meter's `:power-w` (number / lambda / `'symbol`)   |
| `(set-solar-sunlight ID VAL)`          | drive a solar inverter's `:sunlight-pct` (same polymorphism)|
| `(set-component-health ID K)`          | flip health to `'ok` / `'error` / `'standby`             |
| `(set-component-telemetry-mode ID K)`  | `'normal` / `'silent` / `'closed`                        |
| `(set-component-command-mode ID K)`    | `'normal` / `'timeout` / `'error`                        |
| `(set-active-power ID W &key :lifetime-s :clamp)` | gRPC-style setpoint; `:lifetime-s` = lifetime in seconds (the old positional `LIFETIME-MS [CLAMP]` still works and warns), non-nil `:clamp` clamps into the live envelope instead of rejecting, and an empty envelope yields 0 |
| `(set-reactive-power ID VAR &key :lifetime-s :clamp)` | same for the reactive axis; `:clamp` pulls into the gateway's setpoint envelope (own PF / kVA band at the live P ∩ live augmentations ∩ children's Q bands), falling back to the component's own band when no child reports one; an empty envelope yields 0 |
| `(augment-active-bounds ID BOUNDS &key :lifetime-s)` | gRPC-style bounds augmentation: BOUNDS is `(LO HI)` or a list of bands, e.g. `'((-10000 -1000) (1000 10000))` for an exclusion zone; nil edge = unbounded; `:lifetime-s` = lifetime in seconds (default `default-augment-lifetime-s`; the old positional `LIFETIME-MS` still works and warns) |
| `(augment-reactive-bounds ID BOUNDS &key :lifetime-s)` | same for the reactive axis |
| `(set-meter-reactive-power ID VAL)`    | drive a meter's `:reactive-power-var` (number / lambda / `'symbol`)  |
| `(set-meter-power-factor ID PF &OPTIONAL LEADING)` | drive a meter's `:power-factor` (true cos φ in `(0, 1]`); non-nil LEADING negates the derived Q |
| `(plug-ev ID PRESET &rest OVERRIDES)`  | plug a preset car (`'phev` `'city` `'sedan` `'van`) into a charger; overrides `:soc :target-soc :phases :max-current-a :capacity-kwh :taper-start :taper-floor` |
| `(unplug-ev ID)`                       | unplug the car                                            |
| `(ev-info ID)`                         | the car as a plist (`:preset :soc :state …`), or nil      |
| `(ev-presets)`                         | the catalog as a list of plists (`:name :phases :max-current-a :capacity-kwh`) |

Site weather is a singleton, not a per-component knob (see AGENTS.md),
so it isn't in the table above — but `(pass-cloud …)` is the door a
scenario cue reaches for to script a passing cloud over the array:

```lisp
(pass-cloud 80 600 60)  ; 80% depth, 10 minutes, 1-minute ramp in/out
```

It needs weather installed first — `(make-weather)` gives the default
06:00–20:00 UTC clear-sky day — and only bites a solar inverter that is
following the sky (no `:sunlight-pct` of its own); one driven by
`set-solar-sunlight`, like `examples/berlin-demo.lisp`'s PV, is Manual
and ignores it.

`(set-meter-power 100 (lambda () (csv-lookup …)))` and
`(set-meter-power 100 'consumer-power)` install the lambda or
the symbol as the source — the scheduler re-resolves it once per
tick. An imperative numeric `set-meter-power` collapses any prior
dynamic source back to a constant.

Inside a running scenario the five knob setters — `set-meter-power`,
`set-meter-reactive-power`, `set-meter-power-factor`,
`set-solar-sunlight`, `set-boiler-demand`, plus the three clears —
`clear-meter-power`, `clear-meter-reactive` and
`clear-solar-sunlight` — and a charger's plug state — `plug-ev` and
`unplug-ev`, which `set-battery-soc` on a charger writes through
too — are TRANSIENT: the knob's previous value is
captured the first time the run touches it, and `(scenario-stop)`
puts it back. The other stimuli here are not: `set-battery-soc` on
a battery, `set-boiler-pressure`, the health / mode setters and the
setpoint doors all write permanently, inside a run or outside it. Outside a
scenario the knob setters are permanent too, as before. See
[Teardown](#teardown).

`(set-component-telemetry-mode 200 'silent)` plus
`(set-component-command-mode 200 'timeout)` simulates a
"flaky network" — the inverter keeps producing power and the
physics keeps simulating, but the gRPC stream goes quiet and
SetPower requests hang. Useful for exercising downstream apps
that need to cope with stale or unresponsive sources.

Two more defuns wrap the reactive setters above for a
`define-scenario`'s `:drive` section. Unlike everything in the table
above, these aren't setters themselves — bare at the REPL they just
build a plist and touch nothing; only `scenario--run`, walking the
`:drive` list, calls the setter they name:

| Defun                                      | Effect                                                        |
|---------------------------------------------|----------------------------------------------------------------|
| `(drive-meter-reactive ID SOURCE)`          | `:drive`-section wrapper; compiles to `set-meter-reactive-power` |
| `(drive-meter-pf ID PF &OPTIONAL LEADING)`  | `:drive`-section wrapper; compiles to `set-meter-power-factor`  |

## Helpers in `sim/scenarios.lisp`

These helpers are built into the binary. Macrocosim evaluates the
`sim/*.lisp` preludes at startup, before your config runs, so no
explicit `(load …)` is needed:

- `(random-uniform LOW HIGH)` — uniform float in `[LOW, HIGH)`.
- `(random-pick LIST)` — one element of `LIST`, uniformly. `nil`
  on empty.
- `(random-outage IDS &rest opts)` — recurring random outages on
  a random pick from `IDS`. Plist opts: `:min-every-s` /
  `:max-every-s` (gap seconds), `:min-duration-s` / `:max-duration-s`
  (outage seconds), `:kind` (health symbol while down — default
  `'error`). Each transition lands as a journal event. A chain
  started while a scenario is running belongs to that scenario and
  stops with it — including putting the victim's health back if the
  stop lands mid-outage; one started outside any run keeps going
  until `reset-state`.

## Teardown

`(scenario-stop)` ends a run and unwinds it:

- **Cancels the run's own timers** — the agents it installed, the
  `cue` / `expect` timers it scheduled, and a `random-outage` chain
  started while it was running — and if the stop lands mid-outage,
  that chain's victim gets its health put back as the chain is
  cancelled, since the timer that would have restored it is the one
  being cancelled. A chain armed BEFORE the run (a config's own
  top-level `(random-outage …)`, say) is ambient: it keeps going,
  restores its own victims on schedule, and only `reset-state`
  stops it.
- **Restores every driven knob** — a meter's `:power-w` /
  `:reactive-power-var` / power factor, a solar inverter's
  `:sunlight-pct`, a boiler's `:demand` and a charger's plugged car go
  back to what they were the moment before the run first touched
  them — a car the run plugged comes back out, one it unplugged
  goes back in. First snapshot wins:
  it doesn't matter how often the run re-drove a knob, whether a
  cue re-drove it, or whether you poked it yourself mid-run — from
  the REPL, the UI, or `POST /api/mg/{mg}/component/{id}/drive`, all of
  which snapshot on the same rule. A knob the run never touched is
  never restored.
- Freezes elapsed time and every metric accumulator, and flushes +
  closes any CSV sinks.

Those knobs are the whole of it. State a scenario wrote by
any other means stays written: health flipped by a cue's own
`(set-component-health …)`, an agent's setpoints, and the
stimuli that have no snapshot on ANY door — `set-boiler-pressure`
/ `pressure_bar`, and `set-battery-soc` / `soc_pct` on a battery.
A run that teleports a battery to 10 % SoC leaves it at 10 %. Put
such state back in the scenario itself if the next run depends on
it.

`set-battery-soc` / `soc_pct` on an EV charger is the exception,
and it is not a special case so much as the same rule read
correctly: on a charger that call writes the plugged CAR, so it
takes the plug knob's snapshot exactly as `plug-ev` / `unplug-ev`
do. A charger's teleported SoC is therefore undone by teardown
like the plug itself, whatever order the run moved the SoC and
pulled the plug in. A battery's is not — a battery has no plug
knob to snapshot.

Starting a scenario while another is still running stops the
running one first — `(scenario-start …)` does that itself, so it
holds for a bare start from a script or the REPL as much as for a
`define-scenario` run — and the new run begins from the old one's
pre-scenario state rather than inheriting its displaced knobs and
still-firing timers.

## Reading the report

The reporter exposes:

```sh
curl -s http://127.0.0.1:8801/api/mg/2200/scenario          # lifecycle
curl -s http://127.0.0.1:8801/api/mg/2200/scenario/events   # journal (paginated)
curl -s http://127.0.0.1:8801/api/mg/2200/scenario/report   # aggregate metrics
```

`/api/mg/{mg}/scenario/events` takes `?since=N&limit=M` for incremental
polling — pass back the previous response's `next_event_id`
unchanged.

`/api/mg/{mg}/scenario/report` returns:

| Field                          | Meaning                                                   |
|--------------------------------|-----------------------------------------------------------|
| `scenario_elapsed_s`           | seconds since `scenario-start`; frozen on stop            |
| `peak_grid_w`                  | max active-power on the `grid_power` stream so far         |
| `peak_grid_var`                | max \|reactive-power\| on the `grid_reactive_power` stream so far (tracked by magnitude, not signed max — Q swings both ways) |
| `site_pf_at_peak_var`          | power factor `\|P\| / sqrt(P² + Q²)` at the grid connection point at the instant `peak_grid_var` was recorded (paired against the last P sample, not an independently-peaked P); `null` before any pairable PQ sample, or when P and Q were both 0 at that instant |
| `total_battery_charged_wh`     | sum across batteries; positive DC power → charging        |
| `total_battery_discharged_wh`  | sum across batteries; negative DC power → discharging     |
| `total_pv_produced_wh`         | sum across solar inverters; negative active P → produced  |
| `per_battery`                  | `[{ id, charge_wh, discharge_wh }]`                       |
| `per_pv`                       | `[{ id, produced_wh }]`                                    |
| `soc_stats`                    | `{ mean_pct, median_pct, mode_pct }` over current SoCs   |
| `grid_window_averages`         | `[{ window_start, avg_w }]` — average grid power per 15-min UTC-aligned window, oldest first, ≤ 96 |
| `name`                         | name of the running scenario; `null` before any start     |
| `checks_passed`                | count of passed `(scenario-expect …)` checks, full run    |
| `checks_failed`                | count of failed checks, full run                          |
| `checks`                       | recent check results, oldest first (bounded ring)         |

The site P/Q metrics ride the microgrid loopback's `grid_power` and
`grid_reactive_power` formula streams — the same aggregates the UI's
metrics panel charts — so they follow whatever the microgrid-rs
formula engine resolves as the grid connection point, on any topology
shape. Two consequences: the samples are resampled at ~1 Hz rather
than read off raw telemetry, and a headless stepped run with no
loopback leaves peak tracking idle.

## Recording CSVs

`(scenario-record-csv "csvs")` opens one buffered CSV per
component under `csvs/`, named `<id>-<category>.csv`, with a
uniform 5-column header (`ts_iso, active_power_w,
reactive_power_var, dc_power_w, soc_pct`) — empty cells where a
component doesn't publish that field. Rows write at the 1 Hz
history-sampler cadence.

Components with an active-power envelope (the ones a control app
commands) get two extra files:

- `<id>-setpoints.csv` — the control inputs the component
  received: one row per SetActivePower / SetReactivePower /
  AugmentBounds request (value, resolved TTL, arrival time,
  outcome). Written on each request, not sampled.
- `<id>-bounds.csv` — the effective active-power envelope over
  time, sampled at the same 1 Hz pass as telemetry.

Components with a reactive-power envelope (a different set — an
inverter has one, the battery behind it doesn't) get one more:

- `<id>-reactive-bounds.csv` — the effective reactive-power (Q)
  envelope over time, sampled at the same 1 Hz pass as telemetry.

`(scenario-stop)` flushes and closes all files;
`(scenario-stop-csv)` does the same on demand mid-scenario.
