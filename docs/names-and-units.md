# Names and units

Every number in macrocosim has one unit, and that unit is in its name.
The same quantity has the same name in Lisp, HTTP, the WebSocket,
macroctl and Python. This page lists the rules and then the name of
each quantity on each surface.

## Rules

1. **Units.** Power is W, energy Wh, reactive power VAr, apparent power
   VA, voltage V, current A, frequency Hz. Durations are seconds, and
   fractions are allowed. Percent is 0 to 100, and state of charge is
   a percent too. There are two exceptions: boiler pressure stays in
   bar, and steam mass flow is kg/s.
2. **The unit is in the name of every plain number.** Lisp keywords
   end in `-w`, `-wh`, `-var`, `-va`, `-v`, `-a`, `-hz`, `-pct` or
   `-s`. HTTP and JSON fields end in `_w`, `_wh`, `_var`, and so on.
   macroctl flags look like `--lifetime-s` and `--lower-w`. Rates end
   in `-w-per-s`, `-var-per-s` or `-kg-per-s` (`_per_s` in JSON and
   Python). A Lisp `%` is spelled `-pct`.
3. **Python arguments are typed and have no suffix.** They take
   `Power`, `Energy`, `Percentage`, `ReactivePower`, `ApparentPower`,
   `Voltage`, `Current`, `timedelta` or `datetime`. A bare number
   raises `TypeError`. Where `frequenz-quantities` has no type (ramp
   rates, steam flow, pressure), the argument stays a float and keeps
   a suffix, such as `ramp_rate_w_per_s`.
4. **Points in time are RFC 3339 strings with no unit suffix.** The
   time of an event ends in `_at` (`created_at`, `plugged_at`). A
   window is `start` and `end`. The time of a sample or a record is
   `ts`. The one exception is chart series: a component history's
   `samples` pairs and the `/metrics/history` arrays carry epoch
   seconds as numbers (`t_s`), because they are bulk data.
5. **Times of day** (`:sunrise`, `:sunset`) are an `"HH:MM"` string or
   a number of seconds after midnight. They are UTC.
6. **Ids.** The id of a thing is `id`. A reference to another thing is
   spelled out: `microgrid_id`, `component_id` (Lisp `:component-id`).
   A name that says what the id is for keeps its name: `collision_id`,
   `suggested_id`, `dispatch_id`, `--from`, `--to`. URL placeholders
   (`{mg}`, `{id}`) stay as they are.
7. **Values without a unit keep their names.** This covers ratios,
   power factor, counts, ports, enums and seeds: `:reactive-pf-limit`,
   `:power-factor`, `:phases`, `:grpc-port`, `:seed`. It also covers
   model settings documented as such: `:sigma`, `:mean-rev-rate`.
8. **Time strings.** A keyword that also takes a string with the unit
   inside (`"4min"`, `"500ms"`) keeps its name. A bare number there
   means seconds. This holds for `define-scenario :length` and for the
   `:at` of a timeline entry. `define-scenario :date "YYYY-MM-DD"`
   stays as it is.
9. **A generic `value` field has a `unit` field beside it.** This is
   for fields whose unit depends on the context: knob values, setpoint
   values, samples and the `actual` of a scenario check. The unit
   strings are `"W"`, `"Wh"`, `"VAr"`, `"VA"`, `"V"`, `"A"`, `"Hz"`,
   `"%"`, `"s"`, plus `"kg/s"` and `"bar"` for the steam boiler. The
   reactive unit is written `"VAr"`. The one exception is
   `/metrics/history`: its entries are `{t_s, value}` with no unit,
   because the stream name fixes the unit. `/metrics/latest` carries
   the `unit` of each stream.
10. **Names of things stay.** Stream and metric names (`grid_power`,
    `battery_energy`) and function names (`set-meter-power`,
    `set-frequency`) do not carry a unit. A positional argument whose
    unit does not change keeps its place. Its unit is in the
    docstring and in the tables below.
11. **Display.** The tables macroctl prints may scale to kW, kWh or MW.
    They print the unit of electrical metrics, SoC and humidity.
    Ratios (power factor, THD), temperature, wind, pressure,
    irradiance and an unspecified metric print none.
    Everything a program reads follows rule 1.

An unknown field in a JSON request body answers 422 with a JSON error
that names the field. An unknown field in a query string answers 400.
The WebSocket, the endpoints file, macroctl and Python accept no old
names.

## Power

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Rated band | `:rated-lower-w`, `:rated-upper-w` | | `list` columns `rated_lower_w`, `rated_upper_w` | `rated=(Power, Power)` | W |
| Meter power | `:power-w` (number, lambda or symbol), `(set-meter-power ID W)` | `power_w` in `drive`; knob `meter-power` | | `power: Power` | W |
| Setpoint | `(set-active-power ID W)` | `setpoints` entries carry `value` and `unit` | `set-power ID POWER_W` | `set_active_power(power: Power)` | W |
| Bounds | `(augment-active-bounds ID BOUNDS)` | `envelope.active_w` as `[lower, upper]` | `augment-bounds --lower-w --upper-w` | `bounds=(Power, Power)` | W |
| PV array size | `:array-peak-w` | | | | W |
| Scenario peak | | `peak_grid_w` in the report | | | W |

## Energy

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Battery capacity | `:capacity-wh` | | | `capacity: Energy` | Wh |
| Car capacity | `:capacity-wh` in `plug-ev`, `ev-info`, `ev-presets` | `capacity_wh` in `GET .../component/{id}/ev` | | `plug_ev(capacity: Energy)` | Wh |
| Car energy | `:energy-wh` in `ev-info` | `energy_wh` in `.../ev` | | | Wh |
| Boiler thermal mass | `:capacity-wh-per-bar` | | | `capacity_wh_per_bar` (float) | Wh/bar |
| Boiler heat per kg | `:wh-per-kg` | | | `wh_per_kg` (float) | Wh/kg |
| Scenario totals | | `total_battery_charged_wh`, `total_battery_discharged_wh`, `total_pv_produced_wh`, `charge_wh`, `discharge_wh`, `produced_wh` | | | Wh |
| Energy streams | `grid_energy`, `battery_energy` (stream names) | same names in `metrics/latest` | | `grid_energy()` returns `Energy` | Wh |

## Reactive and apparent power

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Meter reactive power | `:reactive-power-var`, `(set-meter-reactive-power ID VAR)` | `reactive_power_var` in `drive` | | `reactive_power: ReactivePower` | VAr |
| Reactive setpoint | `(set-reactive-power ID VAR)` | `setpoints` entries | `set-reactive-power ID POWER_VAR` | | VAr |
| Reactive bounds | `(augment-reactive-bounds ID BOUNDS)` | `envelope.reactive_var` | `augment-reactive-bounds --lower-var --upper-var` | | VAr |
| Apparent-power cap | `:reactive-apparent-va` | knob `reactive-apparent-va` (unit `"VA"`) | | `reactive_apparent: ApparentPower` | VA |
| Reactive cap as a ratio | `:reactive-pf-limit` | knob `reactive-pf-limit` | | `reactive_pf_limit` (float) | none |
| Power factor | `:power-factor`, `:leading`, `(set-meter-power-factor ID PF)` | `power_factor`, `leading` | | `power_factor` (float) | none |
| Scenario peak | | `peak_grid_var` | | | VAr |
| Power factor at the reactive peak | | `site_pf_at_reactive_peak` | | | none |

## Voltage, current and frequency

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Battery voltage | `:voltage-v` | | | `voltage: Voltage` | V |
| Grid fuse rating | `:rated-fuse-current-a` | | `list` prints `fuse=N A` | `rated_fuse_current: Current` | A |
| Car current limit | `:max-current-a` in `plug-ev`, `ev-info`, `ev-presets` | `max_current_a` in `.../ev` | | `plug_ev(max_current: Current)` | A |
| Nominal frequency | `:nominal-hz` in `set-frequency-model` | | | | Hz |
| Frequency now | `(set-frequency HZ)`, `(current-frequency)` | | | | Hz |

## Percent and state of charge

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Initial SoC | `:initial-soc-pct` | | | `initial_soc: Percentage` | % |
| SoC window | `:soc-lower-pct`, `:soc-upper-pct`, `:soc-protect-margin-pct` | | | `soc_lower`, `soc_upper`, `soc_protect_margin` (`Percentage`) | % |
| SoC now | `(set-battery-soc ID PCT)` | `soc_pct` in `drive` | | `soc` signal returns `Percentage` | % |
| Car SoC | `:soc-pct`, `:target-soc-pct` | `soc_pct`, `target_soc_pct` in `.../ev` | | `plug_ev(soc, target_soc: Percentage)` | % |
| Car taper | `:taper-start-pct`, `:taper-floor-pct` | | | `plug_ev(taper_start, taper_floor: Percentage)` | % |
| Sunlight | `:sunlight-pct` (number, lambda or symbol), `(set-solar-sunlight ID PCT)` | `sunlight_pct` | | `sunlight: Percentage`, or `raw(...)` for a lambda | % |
| Weather peak | `:peak-pct` | `peak_pct` | | | % |
| Cloud depth | `:cloud-depth-pct`, `(pass-cloud DEPTH-PCT DURATION-S [RAMP-S])` | `cloud_depth_pct`, `depth_pct` | | | % |
| Clear-sky level | `clear-sky-pct` in `(weather-status)` | `clear_sky_pct` | | | % |
| Stream jitter | `:stream-jitter-pct` | | | `stream_jitter: Percentage` | % |
| Weather jitter | `:weather-jitter-pct` | | | | % |

## Durations

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Stream interval | `:interval-s` | | | `interval: timedelta` | s |
| Command delay | `:command-delay-s`, `:reactive-command-delay-s` | | | `command_delay`, `reactive_command_delay` (`timedelta`) | s |
| Device delay | `:device-delay-s` | | | | s |
| Request lifetime | `:lifetime-s` on `set-active-power`, `set-reactive-power`, `augment-active-bounds`, `augment-reactive-bounds` | `ttl_s` in setpoint events, `remaining_s` in the component state's `setpoints[]` | `--lifetime-s` (whole seconds) | `lifetime: timedelta` | s |
| Default lifetimes | `(set-default-request-lifetime-s S)`, `(set-default-augment-lifetime-s S)` | | | | s |
| Physics tick and sample lag | `(set-physics-tick-s S)`, `(set-sample-lag-s S)` | | | | s |
| PV weather lag | `:weather-lag-s` | | | | s |
| Cloud timing | `:cloud-mean-gap-s` (0 means no ambient clouds), `:cloud-duration-s`, `:cloud-ramp-s` | `cloud_mean_gap_s`, `cloud_duration_s`, `cloud_ramp_s`, `duration_s`, `ramp_s` | | | s |
| Timers | `(every :interval-s S :call F)`, `(define-controller :every-s S)` | | | | s |
| Random outages | `random-outage :min-every-s :max-every-s :min-duration-s :max-duration-s` | | | | s |
| Timeline segments | `(hold V :for-s S)`, `(ramp :to V :over-s S)` | | | | s |
| Scenario end | `(scenario-end-after-s S)` | `scenario_elapsed_s` in the report | | | s |
| Dispatch duration | | `duration_s` | `dispatch create --duration-s` | | s |
| History window | | `window_s` (query) | | | s |
| Polling | | | `dashboard --interval-s` | `timeout`, `poll`, `for_` (`timedelta`) | s |
| Stepped run | | | `scenario run --step-s` (above 0, default 0.1), `--until-s` | `run_scenario_stepped(step, until: timedelta)` | s |

A scenario time (`:at`, `:length`, `check`'s first argument) is a
number of seconds, a string such as `"4min"`, or a clock time (rule 8).

## Points in time

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Dispatch window | | `start`, `end` | `dispatch create --start` | | RFC 3339 |
| Dispatch record | | `created_at`, `updated_at` | | | RFC 3339 |
| Sample time | | `ts` in `metrics/latest`, `/setpoints` events, scenario events and checks, logs and every WebSocket event that carries a time | | | RFC 3339 |
| Car plugged | `:plugged-at` in `ev-info` | `plugged_at` in `.../ev` | | | RFC 3339 |
| Weather clock | | `now`, and `start`, `end` of each cloud in `events` | | | RFC 3339 |
| Chart series | | `t_s` in `metrics/history` entries, and the first number of each `samples` pair in a component history | | | epoch seconds |
| Times of day | `:sunrise`, `:sunset` | `sunrise`, `sunset` | | | `"HH:MM"` (UTC) |

## Rates

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Active ramp rate | `:ramp-rate-w-per-s` | | | `ramp_rate_w_per_s` (float) | W/s |
| Reactive ramp rate | `:reactive-ramp-rate-var-per-s` | | | `reactive_ramp_rate_var_per_s` (float) | VAr/s |
| Frequency model | `:mean-rev-rate` (1/s), `:sigma` (Hz/sqrt(s)) | | | | model settings |

## Steam and pressure

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Steam demand | `:demand-kg-per-s`, `(set-boiler-demand-kg-per-s ID KG-S)`, `(drive-boiler-kg-per-s ID SOURCE)` | `steam_demand_kg_per_s` in `drive`; knob `boiler-demand` (unit `"kg/s"`) | | `demand_kg_per_s` (float) | kg/s |
| Pressure | `:target-bar`, `:max-bar`, `:initial-bar` | `pressure_bar` in `drive`, `pressure_target_bar`; knob `boiler-pressure` (unit `"bar"`) | | `target_bar`, `max_bar`, `initial_bar` (floats) | bar |

## Ids

| Quantity | Lisp | HTTP / JSON | macroctl | Python | Unit |
|---|---|---|---|---|---|
| Own id | `:id` | `id` | `snapshot load --id` | `Component.id` | |
| Microgrid | `(make-microgrid :id N)` | `microgrid_id` on every WebSocket event about one microgrid | `--microgrid-id` | `microgrid_id` | |
| Component | `:component-id` on `check` and `scenario-expect`; the item a `drive-*` helper returns carries `:component-id`; `%plug-ev` takes `:component-id` | `component_id` in history, setpoints, WebSocket events, scenario checks and timeline entries; `component_ids` (comma-separated) in the formula query | `list --component-id` | `component_id`, `Check.component_id` | |
| Dispatch | | `dispatch_id` | `dispatch get ID` | | |
| Load collision | | `collision_id`, `suggested_id` | | | |
| Graph edges | | | `connections --from`, `--to` | | |
| gRPC address | | `grpc_addr` | | `MicrogridEndpoint.grpc_addr` | |

A microgrid in the endpoints file is `{"id", "name", "grpc_addr"}`.
The import and snapshot-load request bodies name the new microgrid
`id`.

## Old Lisp names

Lisp files written with the old names still load. Each old name is
converted to the new one and logs `{old} is deprecated; use {new}` the
first time it is seen in the process. The next time macrocosim saves
a managed file's structure, it rewrites that file's generated block
with the new names; `enterprise.lisp`'s generated block is rewritten
the same way. The hand-written script section of a file is never
rewritten, so its old names warn on every boot. Edit the script
sections by hand, and use the warnings as a checklist. Loading never
writes a file. Mixing old and new names in one form is fine. The last
one wins, as before.

Keywords:

| Old | New | Conversion |
|---|---|---|
| `:rated-fuse-current` | `:rated-fuse-current-a` | none |
| `:rated-lower`, `:rated-upper` | `:rated-lower-w`, `:rated-upper-w` | none |
| `:interval` | `:interval-s` | milliseconds to seconds |
| `:power` (meter) | `:power-w` | none |
| `:reactive-power` (meter) | `:reactive-power-var` | none |
| `:capacity` | `:capacity-wh` | none |
| `:initial-soc` | `:initial-soc-pct` | none |
| `:soc-lower`, `:soc-upper` | `:soc-lower-pct`, `:soc-upper-pct` | none |
| `:soc-protect-margin` | `:soc-protect-margin-pct` | none |
| `:voltage` (battery) | `:voltage-v` | none |
| `:command-delay-ms` | `:command-delay-s` | milliseconds to seconds |
| `:device-delay-ms` | `:device-delay-s` | milliseconds to seconds |
| `:reactive-command-delay-ms` | `:reactive-command-delay-s` | milliseconds to seconds |
| `:ramp-rate` | `:ramp-rate-w-per-s` | none |
| `:reactive-ramp-rate` | `:reactive-ramp-rate-var-per-s` | none |
| `:sunlight%` | `:sunlight-pct` | none |
| `:demand` (steam boiler) | `:demand-kg-per-s` | kg/h to kg/s (a number, lambda or symbol) |
| `:peak%` | `:peak-pct` | none |
| `:cloud-rate` | `:cloud-mean-gap-s` | clouds per hour to mean gap in seconds (0 stays 0) |
| `:cloud-depth` | `:cloud-depth-pct` | none |
| `:cloud-duration` | `:cloud-duration-s` | none |
| `:cloud-ramp` | `:cloud-ramp-s` | none |
| `:nominal` | `:nominal-hz` | none |
| `:soc`, `:target-soc` (`plug-ev`) | `:soc-pct`, `:target-soc-pct` | none |
| `:capacity-kwh` (`plug-ev`) | `:capacity-wh` | kWh to Wh |
| `:taper-start` | `:taper-start-pct` | none |
| `:taper-floor` | `:taper-floor-pct` | fraction to percent |
| `:component` (`scenario-expect`, `check`) | `:component-id` | none |
| `every :milliseconds` | `:interval-s` | milliseconds to seconds |
| `define-controller :every-ms` | `:every-s` | milliseconds to seconds |
| `random-outage :min-every`, `:max-every`, `:min-duration`, `:max-duration` | `:min-every-s`, `:max-every-s`, `:min-duration-s`, `:max-duration-s` | none |
| `hold :for`, `ramp :over` | `:for-s`, `:over-s` | none |

The retired pack keywords on `make-ev-charger` (`:capacity`,
`:initial-soc`, `:soc-lower`, `:soc-upper`, `:soc-protect-margin`) go
through the same table. They are ignored with a warning, as they were
before.

Functions and forms:

| Old | New |
|---|---|
| `(set-active-power ID W LIFETIME-MS [CLAMP])`, and the same for `set-reactive-power`, `augment-active-bounds`, `augment-reactive-bounds` (no `CLAMP` there) | `:lifetime-s` and `:clamp` keywords. A bare number in the old slot is milliseconds and warns. |
| `set-default-request-lifetime-ms` | `set-default-request-lifetime-s` |
| `set-default-augment-lifetime-ms` | `set-default-augment-lifetime-s` |
| `set-physics-tick-ms` | `set-physics-tick-s` |
| `set-sample-lag-ms` | `set-sample-lag-s` |
| `(scenario-end-after MINUTES)` | `(scenario-end-after-s SECONDS)` |
| `(set-boiler-demand ID KG-H)` | `(set-boiler-demand-kg-per-s ID KG-S)` |
| `(drive-boiler ID SOURCE)` with kg/h | `(drive-boiler-kg-per-s ID SOURCE)` |

The old defuns keep their old units. `(ev-info ID)`, `(ev-presets)`
and `(weather-status)` read back the new names only (`:soc-pct`,
`:target-soc-pct`, `:capacity-wh`, `clear-sky-pct`, `sunlight-pct`).
