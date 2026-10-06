;; macrocosim runtime helpers — load this from your config file before
;; anything else:
;;
;;   (unless (boundp 'macrocosim-loaded)
;;     (setq macrocosim-loaded t)
;;     (load "sim/common.lisp"))
;;
;; Built on tulisp-async's `run-with-timer` / `cancel-timer`. Lisp's
;; job is to wire the topology and animate the *environment* (per-tick
;; voltage / frequency perturbations, scheduled events). Component
;; physics — ramps, SoC derating, AC/DC conversion — lives in Rust and
;; is reached via the `make-*` constructors and the gRPC API.

;; -----------------------------------------------------------------------------
;; Timer bookkeeping
;; -----------------------------------------------------------------------------

;; Live timer handles created by `every` and friends. Tracked so that
;; reset-state can cancel them on a config reload — otherwise the old
;; callbacks keep firing into a fresh, unrelated microgrid.
;;
;; Process-global by design. If two distinct scenario scripts ever
;; share a process (rare with the current single-Config design),
;; reset-state would cancel both. Macrocosim runs one Config per
;; process, so today there's no namespace conflict; revisit if a
;; multi-config layout shows up.
(unless (boundp 'active-timers)
  (setq active-timers nil))

;; Every entry on `active-timers` is a (FILE . TIMER) cons: FILE is
;; the source file that armed the timer (a string, from
;; `current-source-file`), or nil when it was armed straight from the
;; REPL. The file is what lets a per-file reload cancel just that
;; file's timers and leave every other file's animation running.

(defun cancel-timers ()
  "Cancel every timer tracked on `active-timers` (pushed by `every`,
`scenario-end-after`, and the random-outage chain). Process-wide —
REPL-armed timers included. A whole-world reload calls this
centrally before replaying the files, so scripts do NOT call it
themselves — a script's own call, replayed after another script,
would cancel that script's freshly re-registered timers.
Re-loading one file cancels only that file's timers, via
`cancel-file-timers`.

Also resets `scenario--armed-timers` and BOTH of the random-outage
chain's own variables — the `random-outage--scenario-owned`
ownership latch and the `random-outage--current-victim` in-flight
marker — to nil, if bound (only true once sim/scenarios.lisp has
been loaded).

Every handle the armed list might be holding was just cancelled
above (it's a subset of the same `active-timers` entries), so a
stale reference left there after a whole-world reload would be dead
weight. The chain those entries belonged to is gone too, in both of
its aspects: a NEXT chain must re-decide for itself whether a
scenario owns it rather than inheriting the cut-off run's answer,
and — the sharper one — a victim id left behind by a chain cancelled
MID-OUTAGE must not outlive it. `random-outage--release-victim`
reads that marker under the NEXT chain's ownership, so a stale id
would have a later scenario's stop force-write
`(set-component-health OLD-ID 'ok)` and journal a \"back\" event for
an outage that chain never caused. Clearing it here costs the
cut-off outage its restore — the timer that would have run it was
just cancelled anyway — and a reload rebuilds the world regardless.

Nothing else needs resetting: whether a timer armed after this point
belongs to a scenario is read from the journal at arm time
(`scenario-running-p`), so a reload landing mid-scenario cannot
leave a stale \"we are tracking\" flag behind to capture an ambient
timer."
  (dolist (entry active-timers)
    (cancel-timer (cdr entry)))
  (setq active-timers nil)
  (when (boundp 'scenario--armed-timers)
    (setq scenario--armed-timers nil))
  (when (boundp 'random-outage--scenario-owned)
    (setq random-outage--scenario-owned nil))
  (when (boundp 'random-outage--current-victim)
    (setq random-outage--current-victim nil)))

(defun cancel-file-timers (file)
  "Cancel every timer FILE armed and drop it from `active-timers`.
FILE is a source path as `current-source-file` reports it. Timers
armed from the REPL (car nil) are left alone — they belong to no
file, so no file's reload may take them down. This is what a
per-file reload calls before re-evaluating the file."
  (let (kept)
    (dolist (entry active-timers)
      (if (equal (car entry) file)
          (cancel-timer (cdr entry))
        (setq kept (cons entry kept))))
    (setq active-timers kept)))

(defun cancel-timer-handle (timer)
  "Cancel exactly TIMER (a handle from `run-with-timer` / `every`,
e.g. one this caller stashed itself) and drop its entry from
`active-timers`. Companion to `cancel-file-timers`: that cancels
every timer a FILE armed; this cancels one specific handle
regardless of which file (or no file) armed it — for a caller that
tracks its own handles directly rather than filing by source, like
`scenario--cancel-timers`."
  (cancel-timer timer)
  (let (kept)
    (dolist (entry active-timers)
      (unless (eq (cdr entry) timer)
        (setq kept (cons entry kept))))
    (setq active-timers kept)))

(defun reset-state ()
  "`cancel-timers`, then wipe the active microgrid's components.
For scripts that own their whole world; a script loaded into a
live multi-microgrid engine usually wants plain `cancel-timers`,
which leaves the ambient microgrid's site alone."
  (cancel-timers)
  (reset-microgrid))

;; -----------------------------------------------------------------------------
;; Periodic helper
;; -----------------------------------------------------------------------------

(defun every (&rest plist)
  "Call :call every :milliseconds ms. First firing happens after the
interval has elapsed — not synchronously at load time — so a config
file can put `every` blocks anywhere relative to the topology they
reference.

Optional :args is a list passed as positional arguments to the
callback on every firing — `(every :call 'fire :args (list 1001))`
calls `(fire 1001)` each tick, saving a closing lambda. Defaults
to no extra args.

The handle is pushed onto `active-timers` paired with the file that
armed it, so a reload of that file (or `cancel-timers`) cancels it.
Returns the bare timer handle (not `active-timers` itself) so a
caller that wants to track just its own timer — `scenario--agent`,
via `define-controller` — can stash it directly."
  (let* ((ms (plist-get plist :milliseconds))
         (func (plist-get plist :call))
         (args (plist-get plist :args))
         (secs (/ ms 1000.0))
         (timer (apply 'run-with-timer secs secs func args)))
    (setq active-timers (cons (cons (current-source-file) timer) active-timers))
    timer))

;; -----------------------------------------------------------------------------
;; In-sim controller
;; -----------------------------------------------------------------------------

(defun define-controller (&rest plist)
  "Register an in-sim controller: call the :on-tick lambda every
:every-ms ms (default 100) on Config's refresh loop. A controller is the
closed-loop counterpart to the open-loop drivers — it *senses* live
state and *actuates* in response, modelling an EMS / dispatcher.

Inside :on-tick, read live state with `component-active-power`,
`component-bound-lower`, and `component-bound-upper` (the latter two
report the effective bounds, so they follow any cap a bounds-driving app
has applied), and actuate with `set-active-power` —
pass its CLAMP arg `t` to command \"as much as the live cap allows\"
without tracking the augmentations yourself. eg. a dispatcher that keeps
a battery inverter charging at the highest power the live cap allows:

  (define-controller :id 'ems
    :on-tick (lambda () (set-active-power 300 (component-bound-upper 300) :lifetime-s 2 :clamp t)))

The optional :id is a readability/label. The timer is tracked on
`active-timers` so `reset-state` cancels it on reload.

Cadence vs command-delay: each command takes :command-delay-s to
execute, and while one executes only the newest incoming command waits
for its turn. A controller that re-sends every :every-ms therefore
trails the target by about one delay; re-sending faster than the delay
is safe (it never starves the device), it just wastes commands."
  (let* ((on-tick (plist-get plist :on-tick))
         (req-ms (plist-get plist :every-ms))
         (ms (if req-ms req-ms 100)))
    (every :milliseconds ms :call on-tick)))

;; -----------------------------------------------------------------------------
;; Removed: the UI override journal
;; -----------------------------------------------------------------------------

(defun load-overrides ()
  "Deprecated no-op. UI edits used to be journaled into
microgrids/config.<id>.overrides.lisp and replayed by this call;
macrocosim now saves a microgrid's structure into the microgrid's
own file. Kept so an older config still loads."
  (log.warn "load-overrides is gone; this microgrid predates managed files — use Adopt in the UI"))

;; -----------------------------------------------------------------------------
;; EV chargers
;; -----------------------------------------------------------------------------

(defun plug-ev (id preset &rest overrides)
  "Plug a preset car into the EV charger ID:
`(plug-ev 1004 'sedan :soc 30 :phases 2)'. PRESET is a catalog
symbol — `'phev' `'city' `'sedan' `'van', as listed by
`(ev-presets)'. OVERRIDES are the `%plug-ev' plist keys, each
replacing that preset's value: :soc :target-soc :phases
:max-current-a :capacity-kwh :taper-start :taper-floor. Errors if
ID is not a charger, already has a car, or an override is out of
range — nothing is half-applied. Inside a scenario the plug is
transient: `(scenario-stop)' puts the charger back as it was."
  (apply '%plug-ev (append (list :id id :preset preset) overrides)))

;; -----------------------------------------------------------------------------
;; Scenario helpers
;; -----------------------------------------------------------------------------

(defun scenario-end-after (minutes)
  "Schedule a single-shot timer that runs (scenario-stop) after
MINUTES wall-clock minutes (not seconds — most other DSL ops are
seconds or milliseconds; this one is minutes because the use case
is fixed-duration runs sized in minutes, e.g.
`(scenario-end-after 60)` for a one-hour cap). The handle goes
onto `active-timers` paired with its source file, like `every`'s,
so a reload cancels it."
  (let ((secs (* minutes 60.0)))
    (setq active-timers
          (cons (cons (current-source-file)
                      (run-with-timer secs nil 'scenario-stop))
                active-timers))))
