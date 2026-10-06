;; Minimal topology for the Y0 walking-skeleton example: one microgrid
;; with a grid connection point and a single meter reporting a constant
;; 7 kW. The meter is the grid's sole child, so it's the derived main /
;; PCC meter and `grid_power` tracks its active power.
(make-microgrid
 :id 1
 :topology
 (lambda ()
   (%make-grid-connection-point
    :id 1
    :successors (list (%make-meter :id 2 :power-w 7000.0)))))
