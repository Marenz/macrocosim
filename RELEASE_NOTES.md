# macrocosim 0.2.0 Release Notes

## Summary

Every number now carries its unit in its name, and the same quantity has
the same name in Lisp, HTTP, the WebSocket, macroctl and Python. The
HTTP API has one route tree and one error format. Old Lisp files keep
loading. HTTP, WebSocket, macroctl and Python have no old names, so
clients need the changes listed under Upgrading.

## Upgrading

### HTTP routes

- Everything about one microgrid is under `/api/mg/{mg}/`. The routes
  without a microgrid (`/api/topology`, `/api/component`,
  `/api/weather`, `/api/history`, `/api/setpoints`, `/api/microgrid/*`)
  are gone. So are `/api/scenario`, `/api/scenario/events`,
  `/api/scenario/report` and `/api/scenario/csv...`. They are now
  `/api/mg/{mg}/scenario...`. `/api/scenarios`, `/api/scenarios/stop`
  and `/api/scenarios/{name}/start` stay where they are.
- `/api/mg/{mg}/microgrid/{status,latest,history,formulas}` is now
  `/api/mg/{mg}/metrics/{status,latest,history,formulas}`.
- A component's state, history and setpoints are at
  `/api/mg/{mg}/component/{id}`, `.../history` and `.../setpoints`.
- `POST /api/microgrids/create` is now `POST /api/microgrids`. Saving a
  snapshot is `POST /api/mg/{mg}/snapshots`.
- Every failure is JSON, `{"error": "..."}`. An unknown path answers
  404 and a wrong method answers 405, both with that body. A success
  that has nothing to return answers 204, and no body carries `ok`.

### HTTP fields

Old field names are not accepted. A request body with an unknown field
answers 422 with a JSON error that names the field. A query string with
an unknown field answers 400.

- Component state: `setpoints[].remaining_ms` is `remaining_s`, in
  seconds. `envelope.active` and `envelope.reactive` are
  `envelope.active_w` and `envelope.reactive_var`.
- Drive (`POST .../component/{id}/drive`): `reactive_var` is
  `reactive_power_var`. `steam_demand_kg_h` is `steam_demand_kg_per_s`,
  in kg/s.
- Dispatches: `start_ms` is `start` in the request and the response.
  `end_ms` is `end` in the response; the request has no `end`.
  `create_ms` and `update_ms` are `created_at` and `updated_at` in the
  response. All of them are RFC 3339 strings.
- History and setpoints answer with `component_id`, not `id`. A
  history `samples` pair is `[t_s, value]`, with `t_s` in epoch seconds.
- `metrics/latest`: `ts_ms` of each stream is `ts`, an RFC 3339 string.
- `metrics/history`: each entry is `{t_s, value}`, with `t_s` in epoch
  seconds.
- Logs: `ts_ms` is `ts`, an RFC 3339 string.
- Weather, request and response: `cloud_depth`, `cloud_duration` and
  `cloud_ramp` are `cloud_depth_pct`, `cloud_duration_s` and
  `cloud_ramp_s`. `cloud_rate_per_h` is `cloud_mean_gap_s`, the mean
  seconds between clouds. In a request, `cloud_mean_gap_s: 0` turns
  ambient clouds off. The response has `null` when there are none.
  `pct` is `sunlight_pct`.
- Status (`POST .../component/{id}/status`): `health` no longer takes
  `ready`. Use `ok`.
- Import (`POST /api/microgrids/import`): `mid` is `id`.
- Snapshot load: `as_id` is `id`.
- Formula query: `ids` is `component_ids`.
- Scenarios: `timeline[].component` is `component_id`. The report's
  `site_pf_at_peak_var` is `site_pf_at_reactive_peak`.
- A generic `value` now has a `unit` beside it: knobs, setpoints and
  scenario checks. The reactive unit is `"VAr"`.
- The `unit` of a reactive stream in `history` and `metrics/latest` is
  `"VAr"`. It was `"var"`.
- The `boiler-demand` knob `value` in a component's `knobs[]` is in
  kg/s. It was kg/h, so divide an old value by 3600.

### WebSocket

- `mg_id` is `microgrid_id` on every event that belongs to a microgrid.
- `id` is `component_id` on `sample`, `setpoint` and `knob_changed`.
- `ts_ms` is `ts`, an RFC 3339 string, on every event that carries a
  time.
- `sample`, `setpoint` and `knob_changed` carry a `unit`.
- The `unit` of a reactive `microgrid_sample` is `"VAr"`. It was
  `"var"`.
- The `boiler-demand` `knob_changed` `value` is in kg/s. It was kg/h.

### Endpoints file

`--emit-endpoints` writes `microgrids[].grpc_addr`. It was
`microgrids[].grpc`.

### macroctl

- `set-power --lifetime` is `--lifetime-s`, in whole seconds.
- `set-power --reactive` is removed. Use the new `set-reactive-power`.
- `augment-bounds --reactive` is removed. Use the new
  `augment-reactive-bounds`, with `--lower-var` and `--upper-var`.
- `augment-bounds --lower`, `--upper` and `--lifetime` are `--lower-w`,
  `--upper-w` and `--lifetime-s`.
- `dispatch create --duration` is `--duration-s`.
- `dashboard --interval` is `--interval-s`.
- `scenario run --step`, which took milliseconds, is `--step-s`, in
  seconds. The default is 0.1 and the value must be above 0.
- `scenario run --until` is `--until-s`. It accepts fractions.
- `snapshot load --as-id` is `--id`.
- `list --id` is `--component-id`.
- `dispatch list`, `get`, `create`, `pause`, `resume` and `delete` no
  longer take the microgrid id as an argument. Use the global
  `--microgrid-id N`, for example `macroctl --microgrid-id 2 dispatch
  list`. Without it, the lowest microgrid id is used.
- `--addr` no longer defaults to `http://[::1]:8800`. Without it, the
  gRPC commands ask the UI server (`--ui-addr`) for the address of the
  microgrid's gRPC server. Pass `--addr` to name a server directly.
- Printed tables still scale to kW and kWh. They print the unit of
  electrical metrics, SoC and humidity; ratios (power factor, THD),
  temperature, wind, pressure, irradiance and an unspecified metric
  print none. A failed check in `report` prints the unit of its actual
  value. `list` labels its columns `rated_lower_w` and `rated_upper_w`, and
  `stream` has a unit column.

### Python

- `mg_id` is `microgrid_id` on every public method and class, in the
  sync and the `aio` client.
- `Component.component_id` is `Component.id`. `Check.component` and
  `Scenario.check_metric(component=...)` are `component_id`.
- `MicrogridEndpoint.grpc` is `MicrogridEndpoint.grpc_addr`.
- Builder arguments renamed: `stream_jitter_pct` is `stream_jitter`,
  `reactive_apparent_va` is `reactive_apparent`, `ramp_rate` is
  `ramp_rate_w_per_s` and `reactive_ramp_rate` is
  `reactive_ramp_rate_var_per_s`. In `steam_boiler`, `demand_kg_h` is
  `demand_kg_per_s`, in kg/s, not kg/h. An old name raises `TypeError`
  that names the new one. `steam_boiler` takes `interval`,
  `command_delay` and `ramp_rate_w_per_s` as named arguments.
- In `plug_ev`, `max_current_a` is `max_current`, a `Current`. `soc`,
  `target_soc`, `taper_start` and `taper_floor` take a `Percentage`,
  not a float. `taper_floor` was a fraction; it is now a percentage.
- `run_scenario_stepped(step=...)` takes a `timedelta` and no longer an
  integer in milliseconds.
- A bare number for a typed argument raises `TypeError`: in the
  builders, `drive(...)`, a signal's `set(...)`, `Scenario.at(...)` and
  `Scenario.drive_meter(...)`. `raw(...)`, for a lambda or a symbol, is
  taken by the builders' `power`, `reactive_power` and `sunlight`, by
  `drive(power=...)` and `Scenario.drive_meter(...)`, and by a meter
  power, meter reactive power or sunlight cue in `Scenario.at(...)`.
- `capacity` goes out in Wh for a battery and for `plug_ev`.
- The builder writes the new Lisp names only.

### Lisp

Files written with the old keywords still load. macrocosim converts
each old keyword to its new one and logs `:old is deprecated; use
:new` once per name. The next time it saves a managed file, it writes
the new names into that file's generated block. `enterprise.lisp` gets
them the next time it is saved, which happens when an eval changes a
`*-defaults` list or an enterprise setting. The hand-written script
section of a file is never rewritten, so its old names warn on every
boot. Edit your script sections by hand, and use the warnings as a
checklist. Loading never writes a file. The full list is under "Old
Lisp names" in [`docs/names-and-units.md`](docs/names-and-units.md).

What to change in your own scripts:

- Put the unit in the keyword: `:power-w`, `:capacity-wh`,
  `:initial-soc-pct`, `:sunlight-pct`, `:ramp-rate-w-per-s`,
  `:command-delay-s`, `:interval-s` and so on. Durations that were in
  milliseconds are now in seconds.
- `:cloud-rate` (clouds per hour) is `:cloud-mean-gap-s`, the mean
  seconds between clouds. `0` means no ambient clouds.
- `:demand` of a steam boiler was kg/h. `:demand-kg-per-s` is kg/s.
- The lifetime of `set-active-power`, `set-reactive-power`,
  `augment-active-bounds` and `augment-reactive-bounds` is the keyword
  `:lifetime-s`, in seconds. `:clamp` is a keyword too. The old
  positional `LIFETIME-MS [CLAMP]` still works in milliseconds and
  warns.
- Use `set-default-request-lifetime-s`, `set-default-augment-lifetime-s`,
  `set-physics-tick-s` and `set-sample-lag-s`. The `-ms` names still
  work in milliseconds and warn.
- Use `(scenario-end-after-s SECONDS)`. `scenario-end-after` still takes
  minutes and warns.
- Use `(set-boiler-demand-kg-per-s ID KG-S)` and
  `(drive-boiler-kg-per-s ID SOURCE)`. `set-boiler-demand` and
  `drive-boiler` still take kg/h and warn.
- In scenarios, `every :interval-s`, `define-controller :every-s`,
  `random-outage :min-every-s :max-every-s :min-duration-s
  :max-duration-s`, `hold :for-s` and `ramp :over-s` take seconds. The
  old keywords still work and warn.
- `check` and `scenario-expect` take `:component-id`. The drive helpers
  carry `:component-id`.
- `%plug-ev` takes the charger as `:component-id`. It was `:id`, which
  is not in the rename table, so an old direct call to `%plug-ev`
  fails. `plug-ev` is unchanged.
- When one call gives the same keyword twice, the first one now counts,
  as with `plist-get`. It was the last one. A `make-*` call's own
  keywords still win over its `*-defaults` list, which now comes after
  them.
- `:health 'ready` is no longer accepted. Use `'ok`.
- A wrong value for `:health`, `:telemetry-mode`, `:command-mode`,
  `:operational-mode` or `:idle` gets a new error text, and so does
  one passed to `set-component-health`,
  `set-component-telemetry-mode`, `set-component-command-mode` or
  `set-component-operational-mode`. The text names the type and lists
  the accepted symbols, for example `unknown Health 'bogus'; expected
  one of ok, error, standby`. A string in place of a symbol gets
  `Expected a symbol for Health (one of ok, error, standby), got:
  "error"`. Update code that matches the old text.
- `(ev-info ID)`, `(ev-presets)` and `(weather-status)` read back the new
  names with no old names: `:soc-pct`, `:target-soc-pct`,
  `:capacity-wh` (Wh, not kWh), `clear-sky-pct` and `sunlight-pct`.
  Update code that reads them.

## New Features

- `docs/names-and-units.md` lists the rules and the name of every
  quantity in Lisp, HTTP, macroctl and Python.
- A `*-defaults` list saved into `enterprise.lisp`, and a form the UI
  formats, keep each keyword on one line with its value when the list
  is too wide for one line. Each keyword and each value had a line of
  its own.
- macroctl has `set-reactive-power` and `augment-reactive-bounds`.
- `GET /api/mg/{mg}/component/{id}/ev` reads the car plugged into a
  charger: `plugged`, `presets`, and when plugged `preset`, `soc_pct`,
  `target_soc_pct`, `phases`, `max_current_a`, `capacity_wh`,
  `energy_wh`, `plugged_at` and `state`. It answers 404 for an unknown
  component and 400 for one that is not a charger.
