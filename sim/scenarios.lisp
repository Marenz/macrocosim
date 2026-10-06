;; Scenario helpers — load from a scenario script before using
;; `random-outage` etc.
;;
;;   (load "sim/scenarios.lisp")
;;
;; Built on `run-with-timer` (tulisp-async) and the existing
;; `set-component-health` defun. Adds random-pick / random-uniform
;; helpers for scenario authors who want stochastic events.
;;
;; TEARDOWN POLICY: `(scenario-stop)` returns every driven knob — a
;; meter's power / reactive-power / power-factor override, a solar
;; inverter's sunlight%, a boiler's demand, and a charger's plug
;; state (`plug-ev` / `unplug-ev`) — to its PRE-SCENARIO
;; state, no matter what happened to that knob in between: the
;; scenario's own `drive` section, a `cue` re-driving it, or a manual
;; poke while the scenario was running. Simple and uniform — first
;; snapshot (taken the moment before a scenario first touches a
;; knob) wins, always — and it holds for EVERY door onto those
;; knobs: the Lisp setters, the UI/REPL through them, and the typed
;; `POST /api/mg/{mg}/component/{id}/drive` route, which snapshots on
;; exactly the same first-touch rule. `scenario-stop` also cancels
;; every timer THIS scenario armed for itself — every `agent`
;; (`scenario--agent` -> `define-controller` -> `every`), every
;; `cue` / `expect` check timer (`scenario--at`), and any
;; `random-outage` chain started while it was running — via
;; `scenario--cancel-timers` below, so nothing is left running to
;; re-drive a knob (or flip a component's health) a heartbeat after
;; it's restored. Cancelling an outage chain MID-OUTAGE also puts
;; that victim's health back, since the timer that would have done
;; it is the one being cancelled.
;;
;; What teardown does NOT undo: health, modes and other state a
;; scenario wrote directly — a cue's own `set-component-health`, an
;; agent's setpoints, `set-boiler-pressure`, and `set-battery-soc`
;; on a BATTERY. Only the knobs with a snapshot (those listed above)
;; and the outage chain's own in-flight victim come back. On an EV
;; charger `set-battery-soc` writes the plugged car, so it takes the
;; plug knob's snapshot like `plug-ev` / `unplug-ev` do and IS undone
;; by teardown, in any order relative to the plug itself.
;;
;; Starting a scenario while another one is still running tears the
;; running one down FIRST — `scenario-start` itself does it, so it
;; holds however the new run was started — and the second run begins
;; from the first's pre-scenario state rather than inheriting its
;; displaced knobs and orphaned timers.

;; -----------------------------------------------------------------------------
;; Scenario-owned timer tracking
;; -----------------------------------------------------------------------------
;;
;; A timer is the running scenario's own — and so cancelled by that
;; scenario's `(scenario-stop)' — exactly when it was armed WHILE a
;; scenario was running. That's the whole rule, and it needs no flag
;; of its own: `(scenario-running-p)' (a Rust defun over the same
;; journal `scenario-start' / `scenario-stop' write) is the truth,
;; and `scenario--track-timer' consults it at arm time. So an ambient
;; `every' / `random-outage' chain armed before any scenario started
;; is left alone by teardown, but the SAME kind of chain, if a
;; scenario armed it, has its handle captured for that scenario's own
;; stop to cancel.
;;
;; Three call sites route through this: `scenario--agent' (agents,
;; via `define-controller' -> `every'), `scenario--at' (cues/checks,
;; via `run-with-timer'), and `random-outage--track' (the re-arms of
;; a scenario-owned outage chain — `random-outage--schedule' /
;; `random-outage--fire' / `random-outage--restore' all funnel
;; through that one choke point, and it decides ownership from the
;; latch `random-outage' set once at chain start rather than
;; re-asking per re-arm; see `random-outage--track').

(unless (boundp 'scenario--armed-timers)
  (setq scenario--armed-timers nil))

(defun scenario--track-timer (timer)
  "Record TIMER on `scenario--armed-timers' if a scenario is
currently RUNNING (`(scenario-running-p)'), so
`scenario--cancel-timers' can stop it later. A no-op outside a
running scenario — just returns TIMER unchanged — so a caller can
wrap an arm-a-timer call unconditionally, e.g.
`(scenario--track-timer (run-with-timer ...))', without needing to
know whether a scenario happens to be running right now."
  (when (scenario-running-p)
    (setq scenario--armed-timers (cons timer scenario--armed-timers)))
  timer)

(defun scenario--cancel-timers ()
  "Cancel every timer a RUNNING scenario itself armed — agents, cues,
checks, and any `:setup'-armed `random-outage' chain's live re-arm —
and drop each from `active-timers' too, via `cancel-timer-handle'.
Leaves any other timer running: a user's own `every' /
`run-with-timer' / `scenario-end-after', another file's, or a
`random-outage' chain that was started OUTSIDE a scenario (armed
while no scenario was running, so `scenario--track-timer' never
recorded it here).

If the chain it just cancelled was MID-OUTAGE, the victim's health
is put back first — see `random-outage--release-victim'. Cancelling
the chain removes the very timer that would have restored it, so
without this a stop landing inside an outage window would leave a
component in `error with nothing left to bring it back.

Then clears `random-outage--scenario-owned', the latch a chain sets
once at its start: a chain this just cancelled owns nothing any
more, and the NEXT chain re-decides ownership for itself.
Idempotent: an empty `scenario--armed-timers' (no scenario ever run,
or already cancelled) is a no-op."
  (dolist (timer scenario--armed-timers)
    (cancel-timer-handle timer))
  (setq scenario--armed-timers nil)
  (random-outage--release-victim)
  (setq random-outage--scenario-owned nil))

(defun random-uniform (low high)
  "Pseudo-random float in [LOW, HIGH). Composed from the integer
`(random N)` primitive — scales a 0..N draw to the requested
range. The `1.0 *` coerces the integer division to float."
  (let ((scale 1000000))
    (+ low (/ (* 1.0 (- high low) (random scale)) scale))))

(defun random-pick (items)
  "Return one element of ITEMS chosen uniformly at random. Returns
nil if ITEMS is empty."
  (when items
    (nth (random (length items)) items)))

;; The random-outage--* state below is hoisted to globals so each
;; timer firing sees the same parameters. With same-ctx tulisp-async
;; this is no longer required for closure visibility (timer bodies
;; funcall on the parent ctx and lexical captures survive), but the
;; globals approach is kept here for clarity — and as a consequence
;; only one random-outage chain runs at a time per process. Calling
;; random-outage again replaces the prior chain's parameters; any
;; timer it has in flight on `active-timers` will continue with the
;; new state on its next firing.
;;
;; `random-outage--scenario-owned` is that chain's ownership latch:
;; t when the chain was started while a scenario was running, so the
;; scenario's `(scenario-stop)` cancels it.
;;
;; `random-outage--current-victim` doubles as the chain's IN-FLIGHT
;; flag: `random-outage--fire` sets it, `random-outage--restore`
;; clears it once health is back, so a non-nil value means "a
;; component is down right now and this chain owes it a restore".
;; That is what lets `scenario--cancel-timers` tell a stop landing
;; inside an outage window from one landing in the gap between two.
;;
;; Both are seeded here so `random-outage--track`,
;; `random-outage--release-victim` and `scenario--cancel-timers` can
;; read them before any chain has ever been armed.

(unless (boundp 'random-outage--scenario-owned)
  (setq random-outage--scenario-owned nil))

(unless (boundp 'random-outage--current-victim)
  (setq random-outage--current-victim nil))

(defun random-outage (ids &rest opts)
  "Schedule recurring random outages on a random pick from IDS.

Plist OPTS:
  :min-every    Lower bound on the gap between outages, seconds.
  :max-every    Upper bound on the gap, seconds.
  :min-duration Outage duration lower bound, seconds.
  :max-duration Outage duration upper bound.
  :kind         Health symbol while down (default 'error).

Each cycle picks a random id, schedules a `(set-component-health
ID KIND)` after a uniform-random gap, reverts to 'ok after a
uniform-random duration, and reschedules — so a single
`(random-outage ...)` call drives outages until something cancels
it: `reset-state`'s central `(cancel-timers)` always does, and — if
this call was made while a scenario was running (a `:setup` form, a
cue, or a REPL call mid-run) — so does that scenario's own
`(scenario-stop)`. An outage chain started OUTSIDE a running
scenario (bare REPL call, or a config's own top-level form) is
unaffected by any scenario's stop — only `reset-state` reaches it.

Ownership is decided ONCE, here, and latched on
`random-outage--scenario-owned` for the whole chain. It cannot be
re-derived per re-arm: a chain re-arms itself from its own timer
callbacks, so an AMBIENT chain that happens to re-arm while some
unrelated scenario is running would be adopted by that scenario
mid-flight and killed by its stop — an ambient chain outliving
every scenario is the whole point of the distinction.

Starting a chain also clears `random-outage--current-victim': the
new chain has nobody down yet, and whatever a previous chain left in
flight is not this one's to restore. Leaving it set would hand that
id to THIS chain's ownership — a scenario stop would then force
`(set-component-health OLD-ID 'ok)` and journal a \"back\" event for
an outage this chain never caused. The cost is that replacing a
chain mid-outage abandons the old victim: the previous chain's
pending restore timer, if it survives, now finds no victim and
no-ops (which also stops it double-scheduling onto the new chain)."
  ;; Remembered for the whole chain: every re-schedule happens inside
  ;; a timer callback, where `current-source-file' is already nil —
  ;; and where `(scenario-running-p)' answers about whatever scenario
  ;; happens to be running THEN, not the one that armed the chain.
  (setq random-outage--file (current-source-file))
  (setq random-outage--scenario-owned (scenario-running-p))
  (setq random-outage--current-victim nil)
  (setq random-outage--ids ids)
  (setq random-outage--min-every    (or (plist-get opts :min-every)    60.0))
  (setq random-outage--max-every    (or (plist-get opts :max-every)    300.0))
  (setq random-outage--min-duration (or (plist-get opts :min-duration) 30.0))
  (setq random-outage--max-duration (or (plist-get opts :max-duration) 90.0))
  (setq random-outage--kind         (or (plist-get opts :kind)         'error))
  (random-outage--schedule))

(defun random-outage--track (timer)
  "Track TIMER on `active-timers' (and, if this chain is owned by a
scenario, on `scenario--armed-timers' too — see
`scenario--track-timer'), dropping this chain's previous
(already-fired) handle from BOTH
lists first — without the prune a multi-day run accumulates
thousands of dead one-shot handles that only `reset-state' ever
cleared, and a long-running scenario would similarly pile up stale
entries `scenario--cancel-timers' would redundantly (though
harmlessly) re-cancel. Only one outage chain runs per process (see
above), so a single replacement slot suffices.

`active-timers' entries are (FILE . TIMER) conses, so that prune
compares the cdr; `scenario--armed-timers' entries are bare handles.
The new `active-timers' entry is filed under `random-outage--file'
— the file that STARTED the chain — and not under
`current-source-file', which is nil inside a timer callback: a
chain re-scheduled from its own callback must stay attributable to
its file, or a reload of that file could not cancel it and the
outage rate would double."
  (when (and (boundp 'random-outage--timer) random-outage--timer)
    (let (kept)
      (dolist (entry active-timers)
        (unless (eq (cdr entry) random-outage--timer)
          (setq kept (cons entry kept))))
      (setq active-timers kept))
    (let (kept)
      (dolist (h scenario--armed-timers)
        (unless (eq h random-outage--timer)
          (setq kept (cons h kept))))
      (setq scenario--armed-timers kept)))
  (setq random-outage--timer timer)
  (setq active-timers
        (cons (cons (if (boundp 'random-outage--file) random-outage--file nil)
                    timer)
              active-timers))
  ;; Ownership comes from the latch `random-outage' set at chain
  ;; start, NOT from asking `(scenario-running-p)' again here: this
  ;; runs inside the chain's own timer callbacks, so re-asking would
  ;; hand an ambient chain to whichever scenario happened to be
  ;; running at its next re-arm.
  (when random-outage--scenario-owned
    (scenario--track-timer timer)))

(defun random-outage--schedule ()
  "Schedule the next outage after a uniform-random gap."
  (let ((gap (random-uniform random-outage--min-every
                             random-outage--max-every)))
    (random-outage--track (run-with-timer gap nil 'random-outage--fire))))

(defun random-outage--fire ()
  "Pick a victim, knock out for a uniform-random duration, then
schedule the restore callback."
  (let ((victim (random-pick random-outage--ids))
        (dur (random-uniform random-outage--min-duration
                             random-outage--max-duration)))
    (when victim
      (setq random-outage--current-victim victim)
      (set-component-health victim random-outage--kind)
      (scenario-event 'outage
                      (format "%d down for %.0f s" victim dur))
      (random-outage--track (run-with-timer dur nil 'random-outage--restore)))))

(defun random-outage--restore ()
  "Revert the victim's health and reschedule the next outage.
Clears `random-outage--current-victim' as it goes: that variable is
the chain's in-flight flag, and leaving a stale id there would tell
`random-outage--release-victim' an outage is still running when the
component is long since back."
  (let ((victim random-outage--current-victim))
    (when victim
      (set-component-health victim 'ok)
      (setq random-outage--current-victim nil)
      (scenario-event 'restored (format "%d back" victim))
      (random-outage--schedule))))

(defun random-outage--release-victim ()
  "End an in-flight outage the cancelled chain can no longer end
itself: restore the victim's health, journal it, and clear the
in-flight flag. Called by `scenario--cancel-timers' AFTER the
chain's pending restore timer has been cancelled, so the two cannot
both fire.

Only for a SCENARIO-OWNED chain. An ambient chain is untouched by
any scenario's stop and will run its own restore on schedule; force-
restoring its victim here would cut an outage short that nobody
asked to end, and the chain would still journal the restore again a
moment later.

A no-op when the stop lands in the gap between outages, no victim
— which is the common case, so a scenario that never happened to be
stopped mid-outage sees nothing new. Health flipped by anything
ELSE, a cue's own `set-component-health' or an agent's, is not
restored: teardown puts back the knobs it snapshotted and the outage
it cancelled, not every write a scenario made.

Where those two do overlap, this wins: the write is an
unconditional `'ok', exactly as `random-outage--restore' has always
been, so a scenario that set the CURRENT VICTIM's health by its own
means has that setting overwritten here. Narrow, and pre-existing —
the chain's own restore would have done the same thing a moment
later had it not been cancelled."
  (when (and random-outage--scenario-owned random-outage--current-victim)
    (let ((victim random-outage--current-victim))
      (set-component-health victim 'ok)
      (setq random-outage--current-victim nil)
      (scenario-event 'restored
                      (format "%d back (scenario stopped mid-outage)" victim)))))

;; -----------------------------------------------------------------------------
;; Declarative signal profiles (timeline / hold / ramp)
;; -----------------------------------------------------------------------------
;;
;; Build a piecewise-linear driver as a sequence of segments instead of
;; hand-rolling a `cond` on `(scenario-elapsed)`. `timeline` returns a
;; dynamic source (a lambda re-resolved each tick), so it plugs straight
;; into `set-meter-power` / `set-solar-sunlight`:
;;
;;   (set-meter-power 100 (timeline (hold 2000 :for 60)
;;                                  (ramp :to 50000 :over 10)
;;                                  (ramp :to 2000 :over 10)))
;;
;; Time is relative to `(scenario-start)`; before the first segment the
;; value is its start, after the last it holds the last segment's end.

(defun hold (value &rest plist)
  "Timeline segment: stay at VALUE for :for seconds."
  (list :dur (plist-get plist :for) :from value :to value))

(defun ramp (&rest plist)
  "Timeline segment: move linearly to :to over :over seconds, starting
from :from — which defaults to the previous segment's end value."
  (list :dur  (plist-get plist :over)
        :to   (plist-get plist :to)
        :from (plist-get plist :from)))

(defun timeline--at (rows lastv tt)
  "Value of the ROWS piecewise-linear profile at scenario time TT. Each
row is (tstart tend vfrom vto); past the last row the value holds LASTV."
  (let ((val lastv))
    (dolist (row rows)
      (if (and (>= tt (nth 0 row)) (< tt (nth 1 row)))
          (setq val (+ (nth 2 row)
                       (* (- (nth 3 row) (nth 2 row))
                          (/ (- tt (nth 0 row)) (- (nth 1 row) (nth 0 row))))))))
    val))

(defun timeline (&rest segments)
  "Return a dynamic source (a lambda over scenario time) walking
SEGMENTS — each a `(hold V :for S)` or `(ramp :to V :over S [:from A])`.
A ramp without :from continues from the previous segment's end value
(0 at the start); after the last segment the value holds its end."
  (let ((tstart 0.0)
        (prev 0.0)
        (rows nil))
    (dolist (seg segments)
      (let* ((dur (plist-get seg :dur))
             (from (plist-get seg :from))
             (to (plist-get seg :to))
             (vfrom (if from from prev))
             (tend (+ tstart dur)))
        (setq rows (append rows (list (list tstart tend vfrom to))))
        (setq tstart tend)
        (setq prev to)))
    (let ((lastv prev))
      (lambda () (timeline--at rows lastv (scenario-elapsed))))))

;; -----------------------------------------------------------------------------
;; Section wrappers for `define-scenario`
;; -----------------------------------------------------------------------------
;;
;; Each wrapper builds an introspectable plist (or, for `event`, a
;; thunk) that a `define-scenario` section holds and the runner (§J2)
;; compiles to the existing primitives. Authoring reads directly:
;;
;;   (define-scenario :name "cloud-fade" :schedule 'relative :length "4min"
;;     :drive  (list (drive-meter 100 2000000.0)
;;                   (drive-solar 200 (timeline (hold 100 :for 120)
;;                                              (ramp :to 20 :over 27))))
;;     :agents (list (controller 'ems :every "500ms"
;;                     (lambda () (set-active-power 300 (component-bound-upper 300) :lifetime-s 2 :clamp t))))
;;     :cues   (list (at "60s" (event 'clouds "rolling in")))
;;     :expect (list (check "110s" :component 2 :metric 'active-power
;;                          :approx 1500000.0 :tol 300000.0)))
;;
;; Cue / check times are resolved to seconds by `resolve-time`, which
;; auto-detects a relative offset ("60s") vs a clock time ("14:00").

(defun drive-meter (id source)
  "Drive section: feed meter ID from SOURCE (a constant, a symbol, or a
dynamic source like `timeline`). Compiles to `set-meter-power`."
  (list :kind 'drive-meter :target id :source source))

(defun drive-solar (id source)
  "Drive section: feed solar inverter ID sunlight % from SOURCE (a
constant, a symbol, or a dynamic source like `timeline`). Compiles to
`set-solar-sunlight`; for several inverters use one drive-solar each."
  (list :kind 'drive-solar :target id :source source))

(defun drive-meter-reactive (id source)
  "Drive section: feed meter ID reactive VArs from SOURCE (a constant,
a symbol, or a dynamic source like `timeline`). Compiles to
`set-meter-reactive-power`."
  (list :kind 'drive-meter-reactive :target id :source source))

(defun drive-meter-pf (id pf &optional leading)
  "Drive section: hold meter ID at power factor PF (cos phi, 0..1],
LEADING non-nil for capacitive. Compiles to `set-meter-power-factor`."
  (list :kind 'drive-meter-pf :target id :pf pf :leading leading))

(defun drive-boiler (target source)
  "Drive TARGET boiler's steam demand (kg/h) from SOURCE."
  (list :kind 'drive-boiler :target target :source source))

(defun controller (id &rest args)
  "Agents section: an in-sim controller named ID firing :every TIME
(default \"100ms\"), running the trailing LAMBDA each tick. Compiles to
`define-controller`."
  (let ((every (or (plist-get args :every) "100ms"))
        (on-tick (car (last args))))
    (list :id id
          :every-ms (* 1000 (resolve-time every))
          :on-tick on-tick)))

(defun at (tt action)
  "Cues section: run ACTION (a thunk, e.g. from `event`, or any 0-arg
lambda) at scenario time TT."
  (list :at-s (resolve-time tt) :action action))

(defun check (tt &rest expect-args)
  "Expect section: at scenario time TT, run a `scenario-expect` check
with EXPECT-ARGS (the same plist scenario-expect takes:
:component / :metric / :approx / :tol / :min / :max)."
  (list :at-s (resolve-time tt) :expect expect-args))

(defun event (kind payload)
  "Cue action: a thunk that journals a `scenario-event` when run. Use
inside `at`, e.g. (at \"60s\" (event 'clouds \"rolling in\"))."
  (lambda () (scenario-event kind payload)))

;; -----------------------------------------------------------------------------
;; Runner — compile a scenario's sections to the existing primitives
;; -----------------------------------------------------------------------------
;;
;; `scenario--run` is what both runners (todo §J2) drive: the Rust
;; entrypoint looks a scenario up in the registry and calls this with
;; its section data. It compiles down to the primitives that already
;; exist — `scenario-start`, `set-meter-power` / `set-solar-sunlight`,
;; `define-controller`, `run-with-timer`, `scenario-expect`,
;; `scenario-record-csv` — so there's no separate runner machinery:
;;
;;  - the live runner funcalls this on the wall clock; cue/check timers
;;    fire on the refresh loop.
;;  - the stepped runner funcalls this then advances the sim clock with
;;    `sim_run`; the same timers fire deterministically on sim-time.
;;
;; RECORD-DIR is resolved Rust-side ('csv -> a default dir, a string ->
;; itself, nil -> no recording) since tulisp has no stringp/symbolp to
;; branch on here.

;; `scenario--armed-timers' / `scenario--track-timer' /
;; `scenario--cancel-timers' live in the "Scenario-owned timer
;; tracking" section near the top of this file — `scenario--agent'
;; and `scenario--at' below both route their timers through
;; `scenario--track-timer'.

(defun scenario--drive (d)
  "Install one drive item D — a `drive-meter` / `drive-solar` /
`drive-meter-reactive` / `drive-meter-pf` / `drive-boiler` plist."
  (let ((target (plist-get d :target))
        (source (plist-get d :source))
        (kind (plist-get d :kind)))
    (cond
     ((eq kind 'drive-meter) (set-meter-power target source))
     ((eq kind 'drive-solar) (set-solar-sunlight target source))
     ((eq kind 'drive-meter-reactive) (set-meter-reactive-power target source))
     ((eq kind 'drive-meter-pf)
      (set-meter-power-factor target (plist-get d :pf) (plist-get d :leading)))
     ((eq kind 'drive-boiler) (set-boiler-demand target source))
     (t (error (format "scenario: unknown drive kind %s" kind))))))

(defun scenario--agent (a)
  "Install one agent A — a `controller` plist — as an in-sim
controller, tracking its timer via `scenario--track-timer` so
`scenario--cancel-timers` can stop it at `(scenario-stop)`."
  (scenario--track-timer
   (define-controller :id (plist-get a :id)
                      :on-tick (plist-get a :on-tick)
                      :every-ms (plist-get a :every-ms))))

(defun scenario--at (secs thunk)
  "Schedule THUNK to run once at scenario time SECS (seconds).
The timer goes on `active-timers' — paired with the file that
started the scenario, like every other tracked timer — so a reload
of THAT file cancels it: rebuilding a file mid-scenario must not
let its stale cues and checks fire into the rebuilt world. A
scenario started from the UI or the REPL has no source file, so no
per-file reload cancels it; only a whole-world reload's central
`cancel-timers' does. Also tracked via `scenario--track-timer' so
`scenario--cancel-timers' can stop a cue/check that hasn't fired yet
when the scenario is stopped early."
  (let ((timer (run-with-timer secs nil thunk)))
    (setq active-timers (cons (cons (current-source-file) timer) active-timers))
    (scenario--track-timer timer)))

(defun scenario--run (name seed setup drive agents cues expect record-dir)
  "Compile and start the scenario NAME: reset the journal, seed RNG,
run SETUP, install DRIVE sources + AGENTS controllers, schedule CUES
actions + EXPECT checks as timers, and open recording. Returns NAME.

Two runs cannot overlap, but this does not police that itself:
`scenario-start' below tears down a scenario that is still running
before it opens the new journal, so the guard covers a bare
`(scenario-start ...)' from a script or the REPL just as much as a
`define-scenario' run compiled here. By the time it returns, the
previous run's knobs are restored and its timers cancelled.

`scenario--armed-timers' is reset anyway, for the one case that
teardown cannot cover: a reload that cut a previous run off
mid-flight, leaving handles behind with no running scenario to
attribute them to. That reset must come AFTER `scenario-start',
never before — the teardown inside it cancels exactly the handles on
that list, so clearing it first would silently leave the previous
run's agents and cues firing. Timers armed from SETUP onward are
tracked automatically — `scenario-start' is what makes
`(scenario-running-p)' true, and `scenario--track-timer' reads
that."
  (scenario-start name)
  (setq scenario--armed-timers nil)
  (when seed (set-random-seed seed))
  (when setup (funcall setup))
  (dolist (d drive) (scenario--drive d))
  (dolist (a agents) (scenario--agent a))
  (dolist (c cues)
    (scenario--at (plist-get c :at-s) (plist-get c :action)))
  (dolist (e expect)
    (let ((args (plist-get e :expect)))
      (scenario--at (plist-get e :at-s)
                    (lambda () (apply 'scenario-expect args)))))
  (when record-dir (scenario-record-csv record-dir))
  name)
