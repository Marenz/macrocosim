# macrocosim

A Rust microgrid simulator with a Lisp-driven config DSL. Reimplementation of
microsim where component physics lives in Rust and Lisp's job
is wiring the topology + animating the environment.

## Layout

- `src/lib.rs` — module roots
- `src/runtime.rs` — `MicrogridRuntimes`: the one startup path per
  microgrid (physics, history, gRPC server, UI loopback) and its
  started / failed status
- `src/sim/` — components + scheduler
  - `component.rs` — `SimulatedComponent` trait, `ComponentHandle`,
    `Telemetry`; `component/` holds the capability traits
    (`Controllable`, `DcStorage`, `ReactiveLimits`, and the knob
    traits `MeterDrive`, `SunlightDrive`, `SteamDrive`, `EvPort`),
    each reached through an accessor on `SimulatedComponent`
  - `microgrid_site/` — per-microgrid registry, physics tick, grid state,
    topology (+ `history.rs` sampler, `scenarios.rs` event log)
  - `microgrids.rs` — enterprise registry + per-mg routing
  - `dispatch.rs` — enterprise dispatch store (per-`microgrid_id`, id
    allocator, lifecycle broadcast); backs the dispatch gRPC + UI
  - `bounds.rs` — `VecBounds`, `ComponentBounds` (the list of TTL
    augmentations a `GatewayAxis` holds)
  - `ramp.rs` — `CommandDelay` + `Ramp`
  - `gateway.rs` (+ `gateway/{step,window}.rs`) — `MicrogridGateway`,
    one per microgrid, held by `MicrogridSite`: every Microgrid API
    rule (setpoint validation, request lifetimes, augmentations, the
    gateway delay and ramp, the SoC window and its share). Reached
    through `site.gateway()`; `site.bounds_of` / `site.telemetry_of`
    are the one bounds read every consumer reports
  - `gateway_axis.rs` — `GatewayAxis`, the API half of one power axis
  - `device_axis.rs` — `DeviceAxis`, the hardware half: a FIFO device
    delay and the physical clamp
  - `decay.rs` — `bounded_exp_decay` + `soc_protected_bounds`, plus
    the SoC lifecycle helpers: `SocProtect` is the gateway's SoC
    window, while `sanitize_soc_pct` and `integrate_soc_pct` are
    shared by the battery and the connected car
  - `battery.rs`, `meter.rs`, `grid.rs`, `ev_charger.rs`,
    `inverter/{battery,solar}_inverter.rs`, `steam_boiler.rs`
  - `ev_presets.rs` — the connected car: preset catalog, per-plug
    overrides, taper, draw law; owned by the charger, never a site
    component
  - `marker.rs` — no-physics categories (chp, wind turbine, power
    transformer, breaker); they classify the meters around them and
    pass their children's power up to the meter above
  - `site_import.rs` — microgrid API site-export JSON → `(make-* …)` /
    `(connect …)` forms for `/api/microgrids/import`
  - `graph_adapter.rs` — lifts a site into
    `frequenz-microgrid-component-graph` nodes/edges (validation +
    the formula endpoint behind the formula explorer panel)
- `src/lisp/` — config DSL glue
  - `mod.rs` — `Config` (fields, accessors, reload)
  - `boot.rs` — `Config::new`: interpreter setup, defun registration,
    tulisp-async wiring, background loops
  - `defuns/` — every `register_*` installer, one file per topic
    (clock, scenarios, microgrids, metadata, runtime_modes, …)
  - `microgrid_file.rs` — text format for a managed microgrid file
    (generated block + script section); `parse` / `compose` /
    `render_block`
  - `overrides.rs` — structural-eval persist-on-write: regenerates a
    microgrid's managed file, or `enterprise.lisp` for enterprise-wide
    state
  - `undo.rs` — per-microgrid undo stack over the managed-file rewrites
  - `snapshots.rs` — per-mg snapshot save/load under `snapshots/{id}/`
  - `make.rs` — `(make-*)` constructors via `AsPlist!`
  - `handle.rs` — `ComponentHandle` ↔ `Shared<dyn TulispAny>` round trip
- `src/ui/` — embedded web UI server
  - `mod.rs` — axum router + serve entry points
  - `api.rs` — `ApiError`, the extractor wrappers (`Json`, `Query`,
    `Path`, `Text`, `WsUpgrade`, `Mg`) and the JSON fallbacks
  - `handlers/` — HTTP handlers, one file per topic (topology, eval,
    scenarios, dispatches, …)
  - `state.rs` / `loopback.rs` / `events_ws.rs` — loopback client cache,
    gRPC loopback supervisor, WS event push
- `ui-assets/` — the SPA as hand-rolled ES modules (`app.js` is the
  entry; `topology.js`, `live.js`, `metrics-store.js`,
  `metrics-panel.js`, `inspect.js`, `repl.js`, `routing.js`,
  `dialogs.js`, `editor.js`, … own one concern each;
  `http.js` the failed-response reader (`errorText`, `getJson`);
  `paste-forms.js` the DOM-free let* builder the editor's paste
  evals, binding children before the parents that push into them;
  `metrics-store.js` holds the derived-stream rings + the PF helpers
  and `metrics-panel.js` the floating charts panel that reads them;
  `chart-lib.js` the uPlot gate every chart builder asks first
  (a note in the slot when the vendored global did not load);
  `live.js` owns the live-overlay pure helpers: label
  text, number formatting, the dead band and edge flow; `pill.js`
  owns the node model and canvas renderer both graph canvases draw
  with, and the zoom tiers (full / hero / marker); `hovercard.js`
  the node hover card (pure model + DOM widget); `side-panel.js` the
  floating-card shell every panel (inspector, formulas, metrics,
  weather, REPL, logs, Defaults, Report) opens in — static-markup
  cards are listed in its `STATIC_PANELS`, per-panel width and spawn
  corner in `PANEL_DEFAULTS`; any card docks into the bottom or right
  strip (`#dock-bottom`, `#dock-right`; `STRIPS` holds each strip's
  ids, axis and sizes; `dockPanel(name, edge)` / `floatPanel` /
  `layoutStrip(edge)`; persisted under `mc-panel-dock-<name>` and
  `mc-strip-<edge>`);
  `splitter.js` the drag-to-resize
  handshake the dock strips use; `strip-model.js` their DOM-free
  arithmetic (shares, order, size clamp); `panel-geometry.js` the
  floating cards' DOM-free placement arithmetic (cascade slot pick);
  `vendor/fonts/` the
  vendored IBM Plex faces (OFL))
  - Reactive power reads at parity with active power across the SPA:
    the hover card draws a Q envelope bar under the P one (same
    `hovercard.js` `envelopeBar`, labelled in VAr); the metrics
    panel's Reactive power card charts the `grid_reactive_power`,
    `pv_reactive_power` and `battery_reactive_power` aggregate
    streams, with each chip reading out PF against its own P stream
    and an optional dashed PF overlay on a right-hand scale (both
    from `metrics-store.js` `pfValue` / `pfText`); the inspector's
    knob table offers
    `set-reactive-power` on inverters and `set-meter-reactive-power`
    / `set-meter-power-factor` (with a `leading` checkbox) on meters.
- `tools/ui-smoke/` — Playwright smoke scripts against a live server
  (`MACROCOSIM_UI=http://127.0.0.1:PORT node tools/ui-smoke/live-topology.mjs`
  points it at a server you already run). The e2e half drives the
  Berlin demo (microgrid 2200) from a scratch state dir; `make ui-e2e`
  boots one and runs it, and the `ui-e2e` job in
  `.github/workflows/ci.yml` does the same on pushes to main and PRs
  targeting main.
- `src/server.rs` — `Microgrid` gRPC service
- `src/assets_server.rs` — `PlatformAssets` gRPC service (shared port)
- `src/dispatch_server.rs` — `MicrogridDispatchService` gRPC service
  (store-and-serve dispatch API; CRUD + stream over `sim::dispatch`)
- `src/proto.rs` + `src/proto_conv.rs` — proto include + `Telemetry` →
  `MetricSample`s
- `src/timeout_tracker.rs` — request-lifetime deadlines on the site
  clock, owned by the gateway and expired on the physics tick
- `src/bin/macrocosim.rs` — headless server
- `src/bin/macroctl.rs` — clap-based client CLI
- `sim/common.lisp` — Lisp helpers (`every`, `cancel-timers`,
  `reset-state`); embedded into the binary with `defaults.lisp` +
  `scenarios.lisp` as the prelude
- `examples/berlin-demo.lisp` — self-contained demo world: generated
  topology block + a script section with the environment animation
  and the seven starter scenarios. Boot scripts are optional
  (`macrocosim [script …]`); a bare boot loads worlds on demand via
  `(load …)` / the Microgrids tab, and `--state-dir` anchors
  `microgrids/`, `enterprise.lisp`, `snapshots/`, and relative paths

## Managed microgrid files

Each microgrid lives in one `.lisp` file: a macrocosim-generated
block (`;;; macrocosim:generated` … `;;; macrocosim:end`) holding a
full `(make-microgrid :id … :name … :grpc-port … :topology (lambda ()
…))` — every component as a flat `%make-*` plus `connect` calls,
rewritten from live state on every structural eval — followed by a
hand-written script section that runs after the structure, in that
microgrid's scope, on every load (scenarios, `set-meter-power`
sources, `every` drivers; never component construction — the
generated block owns structure, and constructing more in the script
section collides with it on the next load).
`microgrid_file::{parse,compose,render_block}` split / rejoin /
derive the two sections. `(set-microgrid-name ID NAME)` and
`(set-microgrid-tso ID TSO)` edit the head's own arguments and
persist like any other structural edit; the `:grpc-port` has no
setter on purpose — a bound gRPC address is fixed, so moving one
needs an unload (a later sub-project).

Files load explicitly — `(load "path.lisp")`, a boot-script arg, or
the UI's Load — never implicitly. Loading a second file that declares
an id already owned by a DIFFERENT file is a hard error naming the
owner; re-loading the SAME file re-registers its microgrid in place,
reusing the live site so its running physics / gRPC survive. A
driver-only script — timers perturbing somebody else's world, no
`(make-microgrid …)` of its own — is watched and hot-reloaded per
file just like any other loaded file, but a *whole-world* reload
(the undo/settings-failure path) only replays files that register a
microgrid; a driver-only script sits out of that replay list.

"Load as N" (`Config::load_as`, `POST /api/load-as`) answers that
collision by copying the managed file to `microgrids/N.lisp` and
re-numbering everything the enterprise makes unique: the head's
`:id` becomes N, `:grpc-port` becomes a free one (the original's is
held by its bound gRPC server), and every component in the
`:topology` lambda gets a fresh `:id` off the enterprise allocator,
with each `(connect a b)` moved to match. Without the component
re-mint a copy of a *populated* live microgrid always fails
("component id X is already registered in microgrid Y") — component
ids are enterprise-unique and a generated block pins every one of
them. Unmanaged files are refused: there is no generated block to
re-number mechanically. Only the generated block is re-numbered —
the hand-written script section is the author's and is copied
verbatim, so any component id it names (a `set-meter-power` source,
an `every` driver) still points at the ORIGINAL's components and
has to be hand-fixed after a load-as.

`enterprise.lisp` carries enterprise-wide state: id, timezone,
request-lifetime bounds, the assets/dispatch socket addresses, and
every `*-defaults` plist. `Config::persist_enterprise` rewrites it
whenever an eval touches enterprise-wide state — same two-section
shape as a microgrid file.

Per-microgrid snapshots live under `snapshots/{mg_id}/{name}.lisp`
(`src/lisp/snapshots.rs`) — a frozen copy of that microgrid's managed
file; loading one writes it back over the live file and reloads just
that microgrid. Undo (`src/lisp/undo.rs`) is per-microgrid too: one
stack per mg id over the managed-file rewrites.

The pre-migration overrides journal and `(load-overrides)` replay are
gone. `load-overrides` survives only as a no-op deprecation shim in
`sim/common.lisp` — a warning for any old, unmigrated file that still
calls it ("this microgrid predates managed files — use Adopt in the
UI").

## Architectural rules

- **Lisp wires + animates the environment, Rust does physics.** Every
  component's tick / ramp / SoC is in Rust. Lisp's only verbs are
  `(make-*)` to build the graph and `(every …)` / `(run-with-timer …)` to
  perturb grid state or flip runtime knobs over time.
- **One gateway per microgrid holds every Microgrid API rule.** The
  gRPC service and the Lisp setpoint / augmentation commands talk to
  `site.gateway()`; the components are plain hardware with one command
  input per power axis (`Controllable::set_command`). The gateway
  validates (0 is always accepted; NaN never), arms request lifetimes
  and stamps augmentations on the site clock, expires both on the
  physics tick (`gateway.step` runs in `tick_once` before the
  components), and refuses a command whose site was reset since its
  lookup ("site was reset"). The protocol layer — fault gates,
  over-bound fault injection, the setpoint journal, lifetime windows —
  stays in `server.rs`. Lock order: gateway → registry read guards →
  component locks; the gateway lock is never held across an `.await`.
- **An axis is a `GatewayAxis` in the gateway plus a `DeviceAxis` in
  the component.** The chain is gateway delay → gateway ramp → device
  delay → output. Each step the gateway targets the armed command
  (else the component's `Controllable::idle_value`), clamps it to
  validation envelope ∩ `Controllable::physical_band`, narrows that
  toward 0 by (battery inverter, P) its window share without crossing
  0, ramps, and never leaves the ramp outside the physical band or the
  share — a narrowing is followed at once, a widening is climbed at
  the ramp rate. The validation envelope is the rated band (P) or the
  `ReactiveCapability` at the last measured P (Q) ∩ live
  augmentations; `:reactive-pf-limit` sets `k` in `|Q| ≤ k × |P|` — a
  ratio of apparent quantities, not true power factor. The device
  output is the delayed command clamped to the rated band and the
  current physical band, so a direct `Controllable::set_command` is
  capped too. A health trip snaps both to 0, empties the delay line,
  and clears the command unless
  `Controllable::keeps_command_through_fault` (PV active, a charger
  with `:resume-on-recovery`). The EV charger's axis produces the
  *limit* it offers the plugged car, not the draw.
- **The battery is hardware from 0 % to 100 %; the SoC window is the
  gateway's.** The battery's `tick` clamps the summed inverter pushes
  to the rated band, refusing charge at 100 % and discharge at 0 %.
  The gateway throttles each battery's bounds from its SoC
  (`:soc-lower`, `:soc-upper`, `:soc-protect-margin`, taper base 1.2,
  floor 0.3) and, each step, shares the room between the inverters
  pushing into it in proportion to their pushes (same sign only), so a
  running setpoint tapers and holds inside the window within one
  device delay. Inverters still publish their push scaled by each
  child's `DcStorage::dc_accept_ratio`, which stays 1 inside the
  window; without the gateway a battery charges to 100 %. Every
  inverter pushing into a battery gets the same ratio, so inverters on
  one bus share a clip in proportion to their pushes. An inverter
  publishes 0 (P and Q) when it is tripped or when no healthy child
  took its push. Reactive power terminates at the inverter: a DC bus
  carries no Q. A meter's own reactive source is the real thing:
  mutually-exclusive `:reactive-power-var` (a VAr constant, lambda, or
  symbol) or `:power-factor` + `:leading` (true cos φ in `(0, 1]`,
  deriving `Q = P·tan(acos(pf))` off the meter's own live P, negated
  when leading). Like `:power-w`, a fixed numeric reactive source
  freezes into the persisted managed file; a lambda or symbol source
  doesn't, and leaves the meter unrenderable.
- **Single physics tick, registration order = tick order.** `MicrogridSite::spawn_physics`
  runs one `tokio::time::interval` at `physics_tick_ms` and calls `tick()` on
  every component in registration order. Children register first because Lisp
  evaluates `:successors` before the surrounding `make-*`.
- **Telemetry stream cadence is anchored to a target timestamp.** `next_due +=
  step` then `sleep until next_due`; re-anchor only when behind. Per-stream
  `:stream-jitter-pct` perturbs each step; mean is exactly the configured
  interval.
- **Site weather is one singleton per microgrid, not per-component.**
  `(make-weather …)` installs a parametric clear-sky day (a sunrise/sunset
  window, a sine peaking at `:peak%`) plus an optional ambient cloud
  generator; `(set-weather …)` retunes any of it in place; `(pass-cloud
  DEPTH DURATION &optional RAMP)` scripts one deterministic cloud;
  `(weather-status)` reads the sky back as an alist (`src/lisp/defuns/weather.rs`,
  `src/sim/weather.rs`). A solar inverter with no `:sunlight%` follows the
  site's weather (`:weather-lag-s` / `:weather-jitter-pct` lag and roughen
  the sample it reads — Follow-only; with no explicit lag each inverter
  gets a stable id-derived 0–60 s offset so a cloud sweeps across a
  multi-PV site, and `:weather-lag-s 0` opts out; `:array-peak-w` sizes
  the DC array whichever source the sunlight comes from); passing
  `:sunlight%` explicitly makes it Manual instead, and
  `(clear-solar-sunlight ID)` is the way back to Follow.
  `GET`/`POST /api/mg/{mg}/weather` mirror the same four doors for the
  weather panel (day curve, live site-% readout, pass-a-cloud
  trigger), which has no Lisp console of its own.

## HTTP API

- Whole-site routes live under `/api/` (microgrids, load, load-as,
  import, eval, format, defaults, logs, clock, scripts, scenarios).
  `POST /api/eval` evaluates with no microgrid in scope.
- Everything about one microgrid lives under `/api/mg/{mg}/`, served
  by one nested router; the `Mg` extractor resolves `{mg}` and
  answers 404 `microgrid {mg} not registered` itself.
- A path names a resource; a verb appears only for an action (start,
  stop, load, load-as, import, adopt, undo, redo, drive,
  snapshots/load, a dispatch's active). Ids that pick a microgrid,
  component or dispatch are path segments; other query parameters
  filter, window or tune the reply (`metric`, `window_s`, `since`,
  `limit`, `width`, `dir`).
- Every failure is JSON `{"error": "..."}` with a 4xx/5xx status
  (`ApiError` in `src/ui/api.rs`); a route may add fields beside it.
  An unknown path answers 404 `no route for {METHOD} {path}`. A known
  path asked with the wrong method answers 405 with the same `no
  route for …` text.
  Success bodies carry no `ok`; an action with nothing to return
  answers 204.

## Build / run / test

```sh
cargo build
cargo test                                # unit tests for bounds/ramp/decay
cargo run --bin macrocosim examples/berlin-demo.lisp
cargo run --bin macroctl -- info
cargo run --bin macroctl -- tree
cargo run --bin macroctl -- stream 1001 --samples 5
cargo run --bin macroctl -- set-power 1001 5000
```

`ui-assets/` changes: `make ui-test` runs biome over `ui-assets`
(config in `biome.json`; `check` also runs the organizeImports assist,
so an unsorted import or export specifier list fails as an error, and
`noDescendingSpecificity` is off for `style.css` in the `overrides`
block until its rules are reordered with a browser to check), then
every `tools/*-test.mjs` plus `boot-smoke.mjs`,
which imports app.js under a DOM shim and catches TDZ / cycle /
bad-export breakage a curl-200 can't see. `make ui-e2e` runs the
Playwright smoke in `tools/ui-smoke/` against a scratch server on
OS-chosen ports, the way CI does; `make ui-e2e-deps` installs its one
dependency, Playwright with a matching Chromium, into the gitignored
`node_modules`.

UI input convention: a numeric field that commits on Enter (inspector
knobs, weather config fields) must hide the browser's native spinner
arrows (`appearance: textfield` + the `-webkit-*-spin-button` rules in
`style.css`) and ignore wheel scrolling (a non-passive `wheel` listener
calling `preventDefault`, Firefox steps a focused number field on a
wheel) — either affordance changes the value without committing it, and
the next poll or blur silently reverts. Arrow KEYS keep stepping: those
are deliberate keyboard edits, one Enter from a commit. Fields that
commit via a button (dialogs, pass-a-cloud) may keep both.

Each registered microgrid binds its own gRPC port; the first
defaults to `[::1]:8800` and subsequent microgrids step by ten
(`:8810`, `:8820`, …), skipping the assets and dispatch ports; an
explicit `:grpc-port` on either is refused. Override via `:grpc-port`
on `(make-microgrid …)`. `MicrogridRuntimes` starts every registered
microgrid at boot and every one `make-microgrid` announces later. A
bind failure, or a server that ends, marks only that microgrid failed
and the process stays up (a boot-time bind failure still exits the
binary); registering it again (reload, undo, snapshot restore, load)
retries. A microgrid's address is fixed after its first successful
bind; a reload adopts a changed `:grpc-port` only for one that never
bound. `/api/microgrids` entries carry `runtime: {status, grpc_addr,
error} | null`, and create / import / load-as / snapshot-as-new wait
for the start and report it; the microgrid card shows the address and
a "failed" chip. The UI's loopback client reads a private copy of the
Microgrid service through an in-memory channel, so a microgrid's live
data reaches the UI whether or not its public port is bound.
macroctl's global `--microgrid-id` selects the microgrid for the
gRPC commands, `dispatch`, `snapshot`, `pool`, `dashboard` and the
scenario readouts (`summary`, `report`, `events`, `run --wait`);
the default is the lowest id. `scenario list` / `load` are
site-wide. `scenario start` / `stop` and a run's start and stop
reach every microgrid's journal; `scenario event` and a scenario's
checks and recordings land on the lowest microgrid, so `--assert`
works only on that one. `--addr` names the gRPC server directly; with
`--microgrid-id` the lookup still runs and the two must agree. The
UI server binds `127.0.0.1:8801` by default; override the
port with `--ui-port N`, or pass `--ephemeral-ports` to bind the UI
and every gRPC / assets / dispatch listener on OS-chosen ports
(parallel CI instances). A routable `--ui-bind` host is still on the
roadmap. Add `--emit-endpoints=PATH` to write the resolved addresses
as one JSON line once bound (the readiness signal).

`PlatformAssets` and `MicrogridDispatchService` each bind a single
shared listener (they're enterprise-wide, keyed by `microgrid_id` per
request): assets on `[::1]:9900`, dispatch on `[::1]:8900`. Override
via `(set-assets-socket-addr …)` / `(set-dispatch-socket-addr …)`.
Point the dispatch CLI at it with
`--url 'grpc://[::1]:8900?ssl=false' --auth-key any` (auth is ignored),
or use `macroctl dispatch {list,create,pause,resume,delete,get}`. The
per-microgrid Dispatches UI sub-tab (`/api/mg/{id}/dispatches`) lists
them and can create / pause / resume / delete; all three write paths
(gRPC, UI, macroctl) funnel through `DispatchStore::{create,set_active}`,
so construction + validation stay identical.

## Dependencies

- `tulisp = { version = "0.29", features = ["sync", "etags"] }` — the
  crates.io release (the git main-branch pin ended when 0.29.0
  shipped). Known 0.29.0 gap: a re-entrant `eval_string` underflows
  the eval-depth counter (fixed upstream) — `src/lisp/defuns/fs.rs`
  works around it with `eval_file`; drop the workaround on the next
  release bump.
- `tulisp-async = "0.1"` — same-ctx timer primitives (`run-with-timer`, `cancel-timer`,
  `sleep-for`). `TokioExecutor::new` calls `Handle::current()`, so
  `Config::new` must be invoked inside a running tokio runtime.
  `register` returns a `Handle`; the pre-tick hook owns one clone
  and ticks it each physics step — without that, no timer body
  ever runs (the same-ctx model has no background firing thread).
- Proto roots are vendored under `submodules/`:
  - `submodules/frequenz-api-microgrid` (pinned at v0.18.1) — override
    with `MACROCOSIM_PROTO_ROOT` for a private mirror.
  - `submodules/frequenz-api-assets` (pinned at v0.1.0).
  - `submodules/frequenz-api-dispatch` (pinned at v1.0.0) — dispatch
    v1; imports the same vendored common v1alpha8, so no common of
    its own.

## Adding a component type

1. New file under `src/sim/` implementing `SimulatedComponent`.
   A component with a capability implements its trait from
   `src/sim/component/` and overrides the matching accessor with
   `Some(self)`: a controllable one implements `Controllable`
   (`has_axis`, `set_command`, `physical_band`, plus the defaulted
   `idle_value`, `initial_value`, `keeps_command_through_fault`,
   `bounds_follow_physical_band`, `gateway_settings`) and keeps one
   `DeviceAxis` per axis; the gateway supplies validation,
   lifetimes, augmentations and the ramp. Add the component to the
   capability table test in `src/sim/component.rs`.
2. Add to `src/sim/mod.rs` re-exports.
3. Add a `%make-foo` defun in `src/lisp/make.rs` with `AsPlist!`-derived
   args, calling `site.register(...)`. Note the leading `%` —
   user-facing topology code calls `make-foo`, which dispatches here.
4. Add a `foo-defaults` plist + `(defun make-foo …)` wrapper to
   `sim/defaults.lisp`. The wrapper `apply`s `%make-foo` to the
   defaults plist `append`-ed in front of the caller's args; AsPlist's
   last-occurrence-wins resolution lets per-component plist values
   override the defaults.
5. (Optional) Override `subtype()` if proto needs `InverterType::Foo` / etc.
6. Add the category to `COMPONENT_MAKE_FNS` in
   `src/lisp/microgrid_file.rs` — the closed set "load as N" uses to
   spot a component form. Miss it and a copy keeps the original's
   ids for that type (`every_component_make_fn_is_a_real_constructor`
   guards the entries, but cannot see a type that was never added).
7. Register through `register_with_modes(...)` so the component gets
   the shared config + runtime kwargs: `:operational-mode` (config,
   persisted; derives the runtime knobs) plus `:health` /
   `:telemetry-mode` / `:command-mode` (runtime fault knobs, checked
   against the operational mode).

## Sample-config DSL convention

Two-layer split:
- `%make-*` — Rust primitives in `src/lisp/make.rs`. Pure plist
  parsing; every field arrives as a plist key, no defaults.
- `make-*` — Lisp wrappers in `sim/defaults.lisp` that prepend a
  `<cat>-defaults` plist and dispatch to `%make-*`.

Topology code uses `make-*` (defaults applied). To opt out of
defaults entirely for one call, invoke `%make-*` directly.
Per-component plist args win without any special handling — AsPlist!
takes the last occurrence of each key and the wrapper's defaults
appear first in the merged plist.

The prelude (common / defaults / scenarios) is compiled into the
binary via `include_str!`, so editing `sim/defaults.lisp` needs a
rebuild; a script that wants live defaults-editing can still
`(load "sim/defaults.lisp")` and `(watch-file …)` it explicitly.

## Lisp value adapters

- Runtime mode enums (`Health`, `TelemetryMode`, `CommandMode`) and
  the config-level `OperationalMode` take their lisp-side
  `TryFrom<TulispObject>` + `TulispConvertible` impls in
  `src/lisp/runtime_modes.rs`. **Symbols only** — `:health 'error`
  works, `:health "error"` errors with a type mismatch. Note the
  split: `OperationalMode` is microgrid CONFIG (persists via the
  structural-eval rewrite of the microgrid's managed file, drives the
  formula engine); the other three are runtime fault knobs that
  depend on it.
- `LispValue` (`src/lisp/value.rs`) — passthrough wrapper that lets a
  raw `TulispObject` ride through `AsPlist!` (works around the
  blanket-`From<T> for T` `Infallible` mismatch). Used for `:power-w`
  and `:sunlight%`, where the make-* dispatcher inspects the raw
  shape to pick between a constant and a `DynamicScalar`.

## Lisp gotchas (current tulisp-vm)

- **Timer bodies run on the calling ctx.** Same-ctx tulisp-async
  funcalls bodies on the parent `TulispContext`, so a lambda's
  lexical captures (`let*`-bound state, the surrounding closure
  environment) are preserved across firings. defuns/defvars/global
  setq results are visible as you'd expect.
- **`(every …)` callbacks fire on `Config`'s dedicated refresh
  loop, not on the physics tick.** `Config::spawn_lisp_refresh_loop`
  ticks on its own 100 ms grid, takes the interpreter lock once
  per pass, refreshes every microgrid's dynamic-scalar inputs,
  then drains the tulisp-async pending-firings mailbox. So a
  `(run-with-timer 0.05 …)` waits up to 100 ms before firing,
  and a zero-delay one fires on the next refresh pass. Tests
  that need a fire without spinning the loop call
  `cfg.refresh_once()` (synchronous wrapper for the same work).
  Physics ticks themselves are pure Rust now — they read the
  atomic scalars the refresh loop has cached and never touch the
  interpreter, so a long `/api/eval` no longer freezes the
  microgrid's beat.

## Adding a runtime knob

1. Field on the component config struct + plist arg in `src/lisp/make.rs`.
2. (If runtime-mutable) a method on the matching capability trait,
   and a Lisp defun in the matching `src/lisp/defuns/` file that
   reaches it through the accessor and broadcasts `note_knob_changed`.
   A knob a scenario can displace also first calls
   `scenario_snapshot_knob` with the `KnobKind` of the slot it writes;
   a new slot needs a new variant, with arms in `snapshot_knob` /
   `restore_knob`. A new capability also needs its own trait, an
   accessor on `SimulatedComponent`, and a row in the capability table
   test. Use `(every …)` or `(run-with-timer …)` from the config to
   script behaviour over time.
3. Demonstrate via a new line in `examples/berlin-demo.lisp` and
   verify via macroctl.

## EV chargers

The charger and the car it charges are two different things
(`src/sim/ev_charger.rs`, `src/sim/ev_presets.rs`). The charger is
config: `(make-ev-charger …)` takes `:phases` (1 or 3 — its own
wiring, which both divides the offered limit into a per-phase current
and caps how many of the car's phases can be used) and `:idle`
(`'paused`, the default, offers nothing with no command standing;
`'full` offers the full rating). It has no pack of its own: the
pack kwargs (`:capacity-wh`, `:initial-soc`, `:soc-lower`,
`:soc-upper`, `:soc-protect-margin`) are accepted with a warning
and ignored, so older files still load, and the site import drops
them from an export's charger.

The car is runtime state, driven by `(plug-ev ID PRESET &rest
overrides)` (`:soc`, `:target-soc`, `:phases`, `:max-current-a`,
`:capacity-kwh`, `:taper-start`, `:taper-floor`), `(unplug-ev ID)`,
`(ev-info ID)` — a plist, or `nil` for an empty charger or any
component that takes no EV — and `(ev-presets)`, the catalog.
`(set-battery-soc ID PCT)` on a charger moves the plugged car's SoC
and errors when nothing is plugged, as does
`POST /api/mg/{mg}/component/{id}/drive` with `soc_pct`. Plug state
is a scenario knob (`KnobKind::Ev`): every write that changes the
plug or the car snapshots it first, so a scenario's teardown puts
the charger back the way it found it.

Being runtime state, the plugged car is never written to the managed
file — `EvCharger::constructor_kwargs` renders `:phases`, `:idle` and
`:resume-on-recovery`, and nothing about the car — so a config that
should start with a car plugged in says so with a `(plug-ev …)` in
the file's hand-written script section, which is preserved verbatim
and re-runs on every load.

## Testing an external bounds-driving app

Macrocosim is used to test apps whose job is to push
`AugmentElectricalComponentBounds` and watch `power_bounds` react (a
GCP active-power limiter is the motivating case).

- **Every gateway-owned axis curtails to its effective (rated ∩
  augmentation) bounds every step.** The gateway re-clamps the
  standing command to the live envelope on each physics tick, so an
  external app narrowing a bound slews the output down at the ramp
  rate, and it recovers when the augmentation relaxes (tests:
  `late_augmentation_re_clamps_an_armed_setpoint`;
  `armed_target_follows_a_tightening_envelope_and_restores`). A
  controller commands a setpoint **once**; it need not re-send.
- The gateway **hard-errors** a command outside the live
  (augmentation-narrowed) setpoint envelope, faithful to the real API
  gateway. A component with children is judged by its reported bounds
  ∩ the children's. A childless one is judged by its validation
  envelope (the rated band, or the reactive capability at the live P,
  ∩ augmentations); a physical band such as the boiler's heat need is
  left out, so a command above it is accepted and held to the need. An
  EMS wanting "max within the cap" reads the bounds and commands
  within them, or uses Lisp `CLAMP`, which clamps into the reported
  bounds, physical band included.
- An inverter set to `:health 'error` (or `'standby`) **trips offline
  to zero output** *and* is dropped from the healthy `power_bounds`
  aggregate. A battery inverter clears its setpoint and awaits
  re-dispatch on recovery; a PV inverter resumes from sunlight; an EV
  charger trips like the battery inverter by default, and
  `:resume-on-recovery t` makes it keep its armed command through the
  fault and ramp back to it on recovery instead; a steam boiler trips
  like the battery inverter too, while its gas burner keeps holding
  pressure at target. A request lifetime expiring on any of them
  (granularity: the physics tick, on the site clock — sim time when
  headless) retargets the ramp instead of snapping it: with a
  `:ramp-rate-w-per-s` the P output moves back to idle at that rate (a PV
  inverter's idle is its sunlight floor), without one it still gets
  there in one tick. The inverters' Q axes ramp at
  `:reactive-ramp-rate-var-per-s`, 2000 VAr/s unless set. A charger's car
  survives a trip; by default charging resumes with the next command,
  and with `:resume-on-recovery t` it ramps back on its own. Every
  command reaches the output after the gateway delay plus a device
  delay (`:device-delay-s`, 0.1 s unless set), each of which lets a
  command out up to the smaller of 5 ms and half the delay early to
  allow for a tick landing early; a command still in the device's
  delay line is dropped by a trip, never replayed.
- Only a component with a gateway axis on the requested side stores an
  augmentation — both inverters on P and Q, the EV charger and the
  steam boiler on P. Every other component or axis (grid, meter,
  battery; the charger's and boiler's Q) answers `UNIMPLEMENTED`, as
  the setpoint path already does for a component that takes no
  setpoint.
- Drive sim state ad-hoc by POSTing lisp to
  `http://127.0.0.1:8801/api/eval`, e.g.
  `--data "(set-component-health 201 'error)"` → `{"value":…}`; a
  failed eval answers 400 `{"error":"…"}`. Use
  `/api/mg/{mg}/eval` to evaluate inside one microgrid's scope.
- Macrocosim's physics supports closed-loop bound tests today; the
  remaining gaps are ergonomic, not physical — scenario assertions,
  an in-sim controller/actor that reacts to live bounds, declarative
  signal profiles, and deterministic sim-time. See `todo.org` §I.

## Roadmap and deferred work

See `todo.org` for the forward-looking roadmap (browser-driven UI
tests, additional gRPC services, physics realism upgrades, CI
end-to-end testing) and known open design questions.
