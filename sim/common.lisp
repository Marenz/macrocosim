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
;; Renamed keywords
;; -----------------------------------------------------------------------------

(defun common--seconds-arg (plist new old &optional divisor)
  "The seconds value in PLIST under key NEW, else the value under the
old key OLD (warning that OLD is deprecated), divided by DIVISOR when
one is given, else nil."
  (or (plist-get plist new)
      (let ((v (plist-get plist old)))
        (when v
          (%warn-renamed (format "%s" old) (format "%s" new)
                         (if divisor " (seconds)" ""))
          (if divisor (/ v divisor) v)))))

;; -----------------------------------------------------------------------------
;; Periodic helper
;; -----------------------------------------------------------------------------

(defun every (&rest plist)
  "Call :call every :interval-s seconds (the old :milliseconds still
works and warns). First firing happens after the
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
  (let* ((secs (common--seconds-arg plist :interval-s :milliseconds 1000.0))
         (func (plist-get plist :call))
         (args (plist-get plist :args))
         (timer (apply 'run-with-timer secs secs func args)))
    (setq active-timers (cons (cons (current-source-file) timer) active-timers))
    timer))

;; -----------------------------------------------------------------------------
;; In-sim controller
;; -----------------------------------------------------------------------------

(defun define-controller (&rest plist)
  "Register an in-sim controller: call the :on-tick lambda every
:every-s seconds (default 0.1; the old :every-ms still works and
warns) on Config's refresh loop. A controller is the
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
for its turn. A controller that re-sends every :every-s therefore
trails the target by about one delay; re-sending faster than the delay
is safe (it never starves the device), it just wastes commands."
  (let* ((on-tick (plist-get plist :on-tick))
         (secs (or (common--seconds-arg plist :every-s :every-ms 1000.0) 0.1)))
    (every :interval-s secs :call on-tick)))

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
`(plug-ev 1004 'sedan :soc-pct 30 :phases 2)'. PRESET is a catalog
symbol — `'phev' `'city' `'sedan' `'van', as listed by
`(ev-presets)'. OVERRIDES are the `%plug-ev' plist keys, each
replacing that preset's value: :soc-pct :target-soc-pct :phases
:max-current-a :capacity-wh :taper-start-pct :taper-floor-pct. Errors if
ID is not a charger, already has a car, or an override is out of
range — nothing is half-applied. Inside a scenario the plug is
transient: `(scenario-stop)' puts the charger back as it was."
  (apply '%plug-ev (append (list :component-id id :preset preset) overrides)))

;; -----------------------------------------------------------------------------
;; Scenario helpers
;; -----------------------------------------------------------------------------

(defun scenario-end-after-s (seconds)
  "Schedule a single-shot timer that runs (scenario-stop) after
SECONDS wall-clock seconds, e.g. `(scenario-end-after-s 3600)' for a
one-hour cap. The handle goes onto `active-timers' paired with its
source file, like `every''s, so a reload cancels it."
  (setq active-timers
        (cons (cons (current-source-file)
                    (run-with-timer seconds nil 'scenario-stop))
              active-timers)))

(defun scenario-end-after (minutes)
  "Like `scenario-end-after-s', with the delay in MINUTES; warns that
it is deprecated."
  (%warn-renamed "scenario-end-after" "scenario-end-after-s" " (seconds)")
  (scenario-end-after-s (* minutes 60.0)))
