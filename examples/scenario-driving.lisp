;; A 30-minute imperative demo scenario, driving the Berlin demo
;; world (load examples/berlin-demo.lisp first).
;;
;; Run from the REPL:
;;
;;   (load "examples/scenario-driving.lisp")
;;
;; The scenario starts on load and runs ONCE: a purely imperative
;; script registers no topology, so it is not replayed on reloads.
;; Watch progress in the UI's "Report" side panel, or curl the JSON
;; endpoints:
;;
;;   curl -s http://127.0.0.1:8801/api/mg/2200/scenario          ;; lifecycle
;;   curl -s http://127.0.0.1:8801/api/mg/2200/scenario/events   ;; journal
;;   curl -s http://127.0.0.1:8801/api/mg/2200/scenario/report   ;; metrics
;;
;; Component ids referenced below match berlin-demo.lisp's pinned
;; topology:
;;
;;   id 2    main meter (the grid's sole child)
;;   id 100  hidden consumer meter (driven by its inline :power-w lambda)
;;   id 200  solar inverter
;;   id 1000 battery, 1001 battery-inverter

;; ── Lifecycle ──────────────────────────────────────────────────
(scenario-start "example-30min")

;; Cap the run at 30 wall-clock minutes; (scenario-stop) fires
;; automatically. It freezes elapsed + every metric accumulator AND
;; unwinds the run: every knob this script drove goes back to what it
;; was before the run first touched it. So the minute-15 sunlight
;; setting is itself reverted at minute 30 — inverter 200 returns to
;; berlin-demo.lisp's own sunlight source, not to the 100 % this
;; script last set — and the outage chain below, armed while the run
;; is in progress, is cancelled with the run. If minute 30 lands
;; inside one of that chain's outages (with the timings below, a
;; battery is down maybe a fifth of the time), teardown puts the
;; victim's health back as it cancels the chain, rather than leaving
;; it in 'error with no chain left to restore it.
;;
;; Health this script flips ITSELF is a different matter: teardown
;; restores driven knobs and the outage chain's victim, not every
;; write. A (set-component-health ...) in a cue of your own stands
;; after the stop until something puts it back.
;;
;; The bare `every` / `run-with-timer` blocks below are NOT the
;; scenario's own timers (only `agent` / `cue` / `expect` sections
;; and a `random-outage` chain are), so they keep firing afterward:
;; the consumer-load loop simply re-drives meter 100 a second after
;; teardown restored it. Cancel them with `(cancel-timers)` when
;; you want the world fully back to where it started.
(scenario-end-after-s 1800)

;; ── Consumer load: end-of-window spike ─────────────────────────
;; Replaces berlin-demo.lisp's gentler inline :power-w profile with a sharper
;; profile: 5 kW base for the first 13 minutes of every 15-minute
;; window, then a 25 kW spike for the last 100 seconds. This is the
;; classic "demand peak right before the billing window closes"
;; stress case.
(every
 :interval-s 1.0
 :call
 (lambda ()
   (let* ((rel (window-elapsed 900.0))
          (base 5000.0)
          (spike (if (> rel 800.0) 25000.0 0.0)))
     (set-meter-power 100 (+ base spike)))))

;; ── PV cloud cover ─────────────────────────────────────────────
;; Drop sunlight to 30 % at minute 10, back to full at minute 15
;; (and back to the world's own source at the minute-30 stop). The
;; solar inverter's `min-avail` clamp picks up each new sunlight%
;; on the next physics tick — observable as a visible drop in
;; available generation on the Report panel.
(run-with-timer 600.0 nil
                (lambda ()
                  (set-solar-sunlight 200 30.0)
                  (scenario-event 'cloud "covered until minute 15")))
(run-with-timer 900.0 nil
                (lambda ()
                  (set-solar-sunlight 200 100.0)
                  (scenario-event 'cloud "cleared")))

;; ── Random battery outages ─────────────────────────────────────
;; Pick a random battery from the list, knock its health to 'error
;; for 60-180 s, repeat with 5-10 minute gaps. Each transition
;; lands as a journal event so the Report panel's event log shows
;; what happened when.
;;
;; Replace the id list with your actual battery ids — `macroctl tree`
;; or the topology JSON (/api/mg/{mg}/topology) is the easiest way to
;; look them up.
(random-outage '(1000)
               :min-every-s 300.0
               :max-every-s 600.0
               :min-duration-s 60.0
               :max-duration-s 180.0
               :kind 'error)

;; ── Silent-but-operational solar inverter at minute 5 ─────────
;; Models a flaky network: the inverter keeps producing power and
;; the physics keeps simulating, but its telemetry stream goes
;; quiet and SetPower requests time out. Useful for testing
;; downstream apps that need to handle stale-streaming sources.
(run-with-timer 300.0 nil
                (lambda ()
                  (set-component-telemetry-mode 200 'silent)
                  (set-component-command-mode 200 'timeout)
                  (scenario-event 'silent "solar 200 stopped streaming")))

;; ── Optional: per-component CSV recording ──────────────────────
;; Uncomment to drop one CSV per registered component into ./csvs/
;; at the 1 Hz history-sampler cadence. (scenario-stop) flushes
;; and closes the files automatically.
;;
;; (scenario-record-csv "csvs")

(scenario-event 'note "example-30min armed")
