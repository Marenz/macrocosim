;; A topology plus a registered scenario, for examples/scenario_gate.py.
;; The scenario DSL (define-scenario / check) is embedded in the binary.
(make-microgrid
 :id 1
 :topology
 (lambda ()
   (%make-grid-connection-point
    :id 1
    :successors (list (%make-meter :id 2 :power-w 5000.0)))))

(define-scenario
 :name        "hold-load"
 :description "The main meter should hold ~5 kW."
 :schedule    'relative
 :length      "3s"
 :expect (list (check "1s" :component-id 2 :metric 'active-power :approx 5000.0 :tol 500.0)))
