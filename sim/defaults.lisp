;; macrocosim per-category defaults + bare-name shorthand DSL.
;;
;; Part of the embedded prelude (compiled into the binary via
;; include_str!), so editing this file needs a rebuild. A script
;; that wants live defaults-editing can `(load "sim/defaults.lisp")`
;; and `(watch-file …)` it explicitly.
;;
;; Two halves:
;;
;;   1. `*-defaults` plists hold the category-wide knobs. Edit a
;;      value here to retune every component of that category in one
;;      place. Per-component plist args still win on each call.
;;
;;   2. `make-*` wrappers (`make-grid-connection-point`,
;;      `make-meter`, `make-battery`, …) are thin `defuns` that call
;;      the matching `%make-*` Rust
;;      primitive with the defaults plist appended *before* the
;;      caller's args. AsPlist's last-wins resolution makes the
;;      per-component plist override category defaults; the `%make-*`
;;      primitives stay available for callers that want zero defaults.

;; -----------------------------------------------------------------------------
;; Per-category defaults
;; -----------------------------------------------------------------------------

(setq grid-defaults
      '(:rated-fuse-current 100
        :stream-jitter-pct  1.0))

(setq meter-defaults
      '(:interval-s        0.2
        :stream-jitter-pct 4.0))

(setq battery-defaults
      '(:soc-protect-margin 10.0
        :stream-jitter-pct  8.0
        :health             ok))

(setq battery-inverter-defaults
      '(:command-delay-s      1.5
        :ramp-rate             5000.0
        :stream-jitter-pct     8.0
        :reactive-pf-limit     0.0         ;; 0 = disabled
        :reactive-apparent-va 32000.0))    ;; kVA-circle envelope

;; Deliberately NO :sunlight% here — an omitted :sunlight% is what
;; makes an array follow the site weather, so adding one to this
;; plist would pin every solar inverter site-wide to a manual
;; constant and silently disable (make-weather).
(setq solar-inverter-defaults
      '(:ramp-rate          2000.0
        :stream-jitter-pct  5.0))

(setq ev-charger-defaults
      '(:command-delay-s     0.5
        :ramp-rate           3000.0
        :stream-jitter-pct   10.0))

;; Steam boiler: hybrid gas/electric — the electric side ramps and
;; delays like the EV charger.
(setq steam-boiler-defaults
      '(:command-delay-s 0.5
        :ramp-rate 50000.0
        :stream-jitter-pct 10.0))

;; The marker categories (chp, wind turbine, power transformer,
;; breaker) carry no physics — one shared default plist.
(setq marker-defaults
      '(:stream-jitter-pct 0.0))

;; -----------------------------------------------------------------------------
;; make-* shorthand wrappers
;; -----------------------------------------------------------------------------
;;
;; Each wrapper prepends its `<cat>-defaults` plist to the caller's
;; args. AsPlist's last-occurrence-wins key resolution makes the
;; per-component plist override the defaults. To bypass defaults
;; entirely for one call, call the `%make-*` primitive directly:
;;
;;   (%make-battery :id 100)                       ; no defaults

(defun make-grid-connection-point
                             (&rest p) (apply '%make-grid-connection-point (append grid-defaults             p)))
(defun make-meter            (&rest p) (apply '%make-meter            (append meter-defaults            p)))
(defun make-battery          (&rest p) (apply '%make-battery          (append battery-defaults          p)))
(defun make-battery-inverter (&rest p) (apply '%make-battery-inverter (append battery-inverter-defaults p)))
(defun make-solar-inverter   (&rest p) (apply '%make-solar-inverter   (append solar-inverter-defaults   p)))
(defun make-ev-charger       (&rest p) (apply '%make-ev-charger       (append ev-charger-defaults       p)))
(defun make-chp              (&rest p) (apply '%make-chp              (append marker-defaults           p)))
(defun make-wind-turbine     (&rest p) (apply '%make-wind-turbine     (append marker-defaults           p)))
(defun make-steam-boiler (&rest p) (apply '%make-steam-boiler (append steam-boiler-defaults p)))
(defun make-power-transformer (&rest p) (apply '%make-power-transformer (append marker-defaults         p)))
(defun make-breaker          (&rest p) (apply '%make-breaker          (append marker-defaults           p)))
