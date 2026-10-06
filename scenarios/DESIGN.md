# Scenarios, revamped — a unified design

Status: proposal. Supersedes the two parallel scenario models that exist today
(`define-scenario` day-stages and ad-hoc `(load …)` driver scripts).

## 1. Why

Today there are **two unrelated scenario systems** plus **three clocks**, and they
don't compose:

- **`define-scenario`** — a registered, named, 24-hour-day model: stages with
  `[hour-from, hour-to)` windows and an `:on` lambda, advanced by a wall-clock
  task. Only these show up in the UI and on the Start button.
- **Driver scripts** — flat `.lisp` files run with `(load …)` from the REPL,
  using `scenario-start`, `set-*`, `every`/`run-with-timer`, and the newer
  `timeline` / `define-controller` / `scenario-expect`. These are the expressive
  ones, but they're invisible to the UI and can't be Started by name.
- **Clocks** — wall-clock (live), the stepped `ManualClock` (headless,
  deterministic), and the relative `scenario-elapsed`. Which one applies is
  implicit and per-model.

Result: the expressive authoring (controllers, timelines, assertions) can't be
run from the UI; the UI-runnable model can't easily express relative-time event
sequences or in-sim agents; and "make it deterministic / fast" is a separate path
again. Picking "wall-clock vs relative vs instant" isn't a knob — it's a fork in
which system you use.

## 2. Core model

Collapse everything to **one** scenario abstraction defined by two orthogonal
axes, so the author picks behaviour instead of picking a system.

### Axis A — schedule reference (how times are written)
- **`relative`** — offsets from scenario start (`"0s"`, `"60s"`, `"3min"`). The
  general case: event sequences, ramps, controller tests.
- **`absolute`** — calendar/wall times (`"14:00"`, a `:date`). For things that
  are inherently time-of-day (TOU tariffs, a day-long demo, "trip at 02:00").
  Today's day-stage model is exactly this, and becomes sugar (§3).

### Axis B — clock driver (how time advances at run time)
- **`real`** — the wall clock; cues fire as real time passes. For anything that
  needs real timing (a live demo, hardware-in-the-loop, watching it happen).
- **`stepped`** — a host-advanced sim clock (the `ManualClock` already wired into
  `Config::new_headless` / `sim_run`); runs deterministically and as fast as you
  step it. For CI assertions and — once the reporting API lands — replaying a
  whole simulated day instantly to populate it.

The axes compose into four useful combinations, all from one authoring model:

|                | real (wall)                     | stepped (sim)                          |
|----------------|---------------------------------|----------------------------------------|
| **relative**   | live event sequence / demo      | deterministic CI test (fast)           |
| **absolute**   | calendar/day scenario, live     | replay a simulated day instantly (→ reporting API) |

The crucial simplification: **authoring is identical across the matrix.** A
scenario is written once; the *runner* chooses the clock driver, and the schedule
reference only changes how cue times are resolved to an instant.

### The scenario itself

A scenario is a registered, named unit with these parts — every part optional, any
value may be a lambda (the escape hatch, so nothing is ever un-expressible):

- **`setup`** — runs once at start (seed RNG, install constant state).
- **`drive`** — continuous environment sources installed for the run (a `timeline`
  on a meter/sunlight, a constant load).
- **`agents`** — in-sim controllers that react to live state each tick
  (`define-controller`), e.g. an EMS dispatching batteries within the live cap.
- **`cues`** — discrete timed actions (`(at TIME action)`): flip health, inject an
  event marker, change a driver.
- **`expect`** — assertions checked at a time, recorded as pass/fail (`scenario-expect`).
- **`record`** — telemetry + received-setpoint/bounds CSVs (already built).

This single model subsumes both of today's systems: day-stages are sugar over
`cues`/`drive`; driver scripts are `setup`+`drive`+`agents`+`cues`+`expect`.

## 3. Authoring DSL

```lisp
(define-scenario
  :name        "cloud-fade"
  :description "PV fades under cloud; the limiter must hold the import cap."
  :schedule    'relative          ; 'relative | 'absolute
  :clock       'real              ; default driver; a runner may override
  :length      "4min"            ; run length (relative) — or :date for absolute
  :seed        42                ; optional; deterministic RNG (with 'stepped)

  :drive (list
    (drive-solar '(200 . 214)
      (timeline (hold 66.6667 :for-s 120) (ramp :to 13.3333 :over-s 27)))
    (drive-meter 100 2000000.0))

  :agents (list
    (controller 'ems :every "500ms"
      (lambda ()
        (dolist (id '(300 301 302))
          (set-active-power id (component-bound-upper id) :lifetime-s 2 :clamp t)))))

  :cues (list
    (at "60s" (event 'clouds "rolling in")))

  :expect (list
    (check "110s" :component-id 2 :metric 'active-power :approx 1500000.0 :tol 300000.0)
    (check "160s" :component-id 2 :metric 'active-power :max 3800000.0))

  :record 'csv)
```

A **wall-clock / calendar** scenario is the same model with `:schedule 'absolute`;
the old day-stage form is kept as sugar that expands to `:drive` + `:cues`:

```lisp
(define-scenario
  :name "european-intraday" :schedule 'absolute :date "2026-06-15"
  :stages (list
    (stage "morning-ramp" :from "06:00" :to "10:00" :on (lambda () …))
    (stage "midday-pv"    :from "10:00" :to "16:00" :on (lambda () …))))
```

A **fully imperative** scenario (nothing stranded) wraps a body:

```lisp
(define-scenario :name "ad-hoc" :schedule 'relative
  :setup (lambda () … arbitrary set-* / every / run-with-timer …))
```

Notes:
- Times are human strings (`"500ms"`, `"60s"`, `"3min"`, `"14:00"`) resolved
  against the schedule reference; numbers still accepted.
- `drive-solar`/`drive-meter`/`controller`/`at`/`check`/`event` are thin wrappers
  over the primitives that already exist (`set-solar-sunlight`, `set-meter-power`,
  `define-controller`, `run-with-timer`, `scenario-expect`, `scenario-event`).
- The whole `define-scenario` form is data: introspectable for the UI (render the
  timeline, list expectations) without running it.

## 4. Execution — runners

A **Runner** = (clock driver, schedule resolver). Authoring doesn't change; the
runner does. Both read the one scenario registry.

- **Live runner** — real wall clock, driven by the existing refresh/auto-advance
  loops. `relative` cues fire at `start + offset`; `absolute` cues at the
  calendar time. For "needs real time" and live demos.
- **Stepped runner** — the headless `ManualClock` path (`Config::new_headless` +
  `sim_run`). Advances the clock in fixed steps, ticking timers + physics +
  scenario time together; deterministic and fast. Surfaced as
  `macroctl scenario run NAME --stepped [--until-s …] [--step-s …] [--assert]` and a UI
  "Run headless" action. `--assert` exits non-zero on a failed check (already
  built) — the CI gate.

A scenario may declare a **required** clock (e.g. a controller-timing test that
only makes sense stepped, or a hardware test that needs real); otherwise the
runner's choice wins, defaulting to `:clock`.

## 5. Clocks

Collapse to one `ScenarioClock { now() -> DateTime, elapsed() -> Duration }`
backed by either wall (`Utc::now`) or the `ManualClock`. `NowSource` (already
introduced) generalizes into it; tulisp-async timers are already clock-pluggable.
The display **timezone** clock is unrelated (presentation only) and stays as-is.

Determinism guarantee: **stepped clock + `:seed`** ⇒ bit-for-bit reproducible
(the seeded RNG already gives reproducible draws; stepping fixes their ordering).
Wall-clock runs are best-effort (stream jitter, scheduler).

## 6. Assertions, reporting, recording

Already built; the model just wires them in declaratively:
- `:expect` → `scenario-expect` checks → the scenario report's pass/fail ledger →
  `macroctl scenario report --assert`.
- `:record 'csv` → per-component telemetry + received-setpoint/effective-bounds
  CSVs.
- The reporting API (future) consumes the same report/CSVs; an `absolute +
  stepped` run is exactly "replay a day instantly to populate reporting."

## 7. UI

Scenarios mode becomes the single home for everything:
- **List** every registered scenario: name, description, schedule type, length,
  required/last clock, last-run pass/fail badge.
- **Per scenario**: *Run (live)* and *Run (headless)* (when stepped-compatible),
  *Stop*. No more "load a driver file" as a separate concept.
- **Run view**: a timeline strip of cues + expectations showing what has fired /
  passed / failed as it progresses, plus live metrics (peak import, SoC, battery
  charge) and the check ledger; links to the CSVs.
- The **REPL** stays for ad-hoc `set-*` and for `(load …)`-ing a file that calls
  `define-scenario` (registration), but running is via the registry.

## 8. Migration

Bold revamp, but nothing stranded:
- **Day-stages** → kept as the `:schedule 'absolute` + `:stages` sugar; existing
  `define-scenario` forms keep working through a shim, or are mechanically
  rewritten.
- **Driver scripts** → wrap the body in `:setup`/`:drive`/`:agents`/`:cues`/
  `:expect`; a `:setup`-only form covers the fully-imperative ones.
- The primitives (`scenario-start/-stop/-event/-expect/-record-csv`,
  `timeline`, `define-controller`, the read primitives) stay — the new model
  compiles down to them. So this is mostly an authoring + registry + runner +
  UI layer over what already exists; the physics/clock/assertion plumbing is done.

## 9. Implementation phases

1. **ScenarioClock** unification (fold `NowSource` in; wall/stepped). *Mostly done.*
2. **New `define-scenario` data model** (`schedule`/`clock`/`seed`/`length` +
   `setup`/`drive`/`agents`/`cues`/`expect`/`record`), compiling to existing
   primitives; old `:stages` form kept as sugar.
3. **Runners off the registry**: live (cues fired by the refresh/auto-advance
   loop against the clock) and stepped (headless `sim_run` over the registry).
4. **macroctl**: `scenario run NAME [--stepped] [--until-s] [--step-s] [--assert]`,
   `scenario list`.
5. **UI revamp**: registry list + Run live/headless + timeline/ledger run view.
6. **Migrate** existing scenarios; retire the ad-hoc load-to-run path.

## 10. Open questions

- **Absolute + stepped** ("replay a day instantly") semantics — defer the precise
  contract until the reporting API lands, but the model already accommodates it.
- **Back-compat window** for the current `define-scenario` stage API — keep the
  shim, or rewrite the handful of existing scenarios and drop it? (Leaning: keep
  the shim one release, then drop.)
- **Agents in wall-clock live mode** — allowed; only determinism differs.
- **Human-time parser** scope — start with `ms`/`s`/`min`/`h` + `HH:MM`; grow as
  needed.
