//! Solar (PV) inverter. Active side: produces a negative power
//! proportional to `sunlight_pct`; the gateway slews it, the device
//! delays it, and less sun cuts it at once, as it would a real array.
//! Reactive side: a second axis, as on the battery inverter — a real
//! PV smart inverter (IEEE 1547-2018) does Volt/VAR control alongside
//! its real-power output.

use std::{
    fmt,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use rand::Rng;
use tulisp::TulispContext;

use crate::sim::{
    Category, Controllable, MicrogridSite, ReactiveLimits, SimulatedComponent, Telemetry,
    bounds::VecBounds,
    component::{GatewaySettings, KnobKind, KnobSnapshot, ScalarReading, SunlightDrive},
    device_axis::DeviceAxis,
    dynamic_scalar::DynamicScalar,
    reactive::ReactiveCapability,
    runtime::Health,
};
use crate::timeout_tracker::SetpointAxis;

/// What a PV inverter's sunlight cache starts at, and what a
/// weather-following inverter therefore reads before its first tick:
/// full sun, so an inverter that has never ticked reads bright rather
/// than dark.
const SUNLIGHT_SEED_PCT: f32 = 100.0;

/// Where a PV inverter's cloud-cover percentage comes from.
///
/// Two shapes, and the split is about *who* produces the number:
///
/// - [`Self::Follow`] — the site's own [`Weather`] does, sampled a
///   little behind `now` and optionally roughened by a per-tick
///   `±cfg.weather_jitter_pct` factor. How far behind is two separate
///   ideas wearing one field: an EXPLICIT `cfg.weather_lag` models
///   thermal/irradiance inertia between the sky and the array, while
///   an unset one stands in for nothing physical — it is the stable
///   id-derived offset (0–60 s, `default_weather_lag`) that makes a
///   cloud sweep across a multi-PV site instead of hitting every
///   array in the same tick. Nobody has driven this inverter's knob;
///   it just tracks the sky.
/// - [`Self::Manual`] — something drove it: a `:sunlight-pct` kwarg, a
///   `(set-solar-sunlight …)` poke, a scenario, the UI. The
///   [`DynamicScalar`] underneath covers both a plain constant and a
///   Lisp expression re-resolved by `refresh_inputs`.
///
/// This is a plain tag: it says which of the two shapes a slot is in
/// and nothing else. The lag and jitter that shape a `Follow` sample
/// live on [`SolarInverterConfig`] (their only writer), and the value
/// `tick` resolves lives in [`SolarInverter`]'s own `sunlight_cache`
/// — so `Clone` is just a derive, and a snapshot of this enum carries
/// the *shape* of the slot, never a copy of the sky.
///
/// `Follow` resolves in [`SimulatedComponent::tick`] because that is
/// the only door handed both the site (which owns the weather) and
/// `now`. The resolved value is cached in an atomic so the *site-less*
/// readers — `sunlight_pct()`, `min_avail_w()`, the inspector's
/// `sunlight_reading()`, all of which have neither a site nor a clock
/// — get the same answer without re-deriving it.
///
/// [`Weather`]: crate::sim::weather::Weather
#[derive(Clone)]
pub enum SunlightSource {
    /// Track the site's weather, at the lag and jitter the inverter's
    /// config carries. Read back through
    /// [`SolarInverter::sunlight_pct`], which serves the cache.
    Follow,
    /// A driven value: a constant, or a Lisp expression re-resolved
    /// each `refresh_inputs`.
    Manual(DynamicScalar),
}

impl SunlightSource {
    /// A driven source wrapping `scalar`.
    pub fn manual(scalar: DynamicScalar) -> Self {
        Self::Manual(scalar)
    }
}

#[derive(Clone, Debug)]
pub struct SolarInverterConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    /// Where the sunlight slot starts, in one field:
    ///
    /// - `None` — follow the site's weather
    ///   ([`SunlightSource::Follow`]), shaped by
    ///   [`Self::weather_lag`] and [`Self::weather_jitter_pct`].
    /// - `Some(v)` — start driven at `v` percent
    ///   ([`SunlightSource::Manual`]).
    ///
    /// Defaults to `Some(100.0)`: an inverter built without saying
    /// anything about weather keeps the historical full-sun constant,
    /// so nothing that never heard of weather changes behaviour. The
    /// Lisp door maps an absent `:sunlight-pct` to `None` — the absent
    /// kwarg *is* how a weather-following inverter renders.
    pub sunlight_pct: Option<f32>,
    pub command_delay: Duration,
    pub ramp_rate_w_per_s: f32,
    pub stream_jitter_pct: f32,
    /// Q envelope. Default microsim-compatible PF cap of 0.35.
    pub reactive: ReactiveCapability,
    /// SCADA / inverter-internal latency before a Q setpoint starts
    /// being tracked. 100 ms default.
    pub reactive_command_delay: Duration,
    /// Reactive slew rate (VAR/s). 2000 default ≈ 5 s OLRT for a
    /// 10 kVAR window — IEEE 1547-2018 Cat B baseline.
    pub reactive_ramp_rate_var_per_s: f32,
    /// True when `:sunlight-pct` was constructed as a lambda or symbol
    /// rather than a plain number. Not a plist kwarg itself — it
    /// only tells the microgrid-file renderer to omit `:sunlight-pct`
    /// (a dynamic source can't round-trip as a static number) rather
    /// than write out `sunlight_pct`'s stale fallback value.
    pub sunlight_dynamic: bool,
    /// How far behind the sky a `Follow` source samples — the array
    /// sees `weather_pct_at(now - lag)`. `None` (the default) uses a
    /// small stable offset derived from the inverter's id (0–60 s),
    /// so a cloud sweeps across a multi-PV site instead of hitting
    /// every inverter in the same tick. An explicit value — zero is
    /// the opt-out — is used exactly as given. This is the only copy
    /// of the number: `resolve_sunlight` reads it on every tick.
    pub weather_lag: Option<Duration>,
    /// Per-tick uniform `±pct` roughening applied to a `Follow`
    /// sample, in percent of the value. Zero by default, and zero
    /// skips the RNG entirely. Read from here on every tick, like
    /// [`Self::weather_lag`].
    pub weather_jitter_pct: f32,
    /// The array's peak DC output (Wp), positive — not an
    /// instantaneous power. Defaults to |rated-lower|: a matched
    /// array. Oversizing produces midday clipping.
    pub array_peak_w: f32,
    /// Time a command takes to reach the output once the inverter has
    /// it, on both axes; default 100 ms.
    pub device_delay: Duration,
}

impl Default for SolarInverterConfig {
    fn default() -> Self {
        Self {
            rated_lower_w: -30_000.0,
            rated_upper_w: 0.0,
            sunlight_pct: Some(100.0),
            command_delay: Duration::ZERO,
            ramp_rate_w_per_s: f32::INFINITY,
            stream_jitter_pct: 0.0,
            reactive: ReactiveCapability::microsim_default(),
            reactive_command_delay: Duration::from_millis(100),
            reactive_ramp_rate_var_per_s: 2000.0,
            sunlight_dynamic: false,
            weather_lag: None,
            weather_jitter_pct: 0.0,
            array_peak_w: 30_000.0,
            device_delay: super::DEFAULT_DEVICE_DELAY,
        }
    }
}

pub struct SolarInverter {
    id: u64,
    name: String,
    interval: Duration,
    cfg: SolarInverterConfig,
    /// Cloud-cover percentage — see [`SunlightSource`]. Either
    /// `Follow` (tracking the site's weather, resolved in `tick`) or
    /// `Manual`: a constant (the cfg default or a numeric
    /// `:sunlight-pct`) or a Lisp expression (`:sunlight-pct (lambda () …)`
    /// / `:sunlight-pct 'symbol`) re-resolved each tick by
    /// `refresh_inputs`. Lisp timers can also push values via
    /// `(set-solar-sunlight ID PCT)`, which collapses any prior
    /// source — dynamic or weather-following — to a constant;
    /// [`Self::clear_sunlight`] is the way back to `Follow`.
    sunlight_source: RwLock<SunlightSource>,
    /// The last cloud-cover percentage `tick` resolved for a
    /// [`SunlightSource::Follow`] slot, as `f32` bits, seeded at
    /// [`SUNLIGHT_SEED_PCT`]. Unread while the slot is `Manual` — a
    /// driven source answers from its own scalar — and left alone by
    /// the setters, so a slot that goes back to `Follow` reads the
    /// last sky until the next tick refreshes it.
    ///
    /// It sits here rather than inside the variant so that reading it
    /// needs no lock on the source, which is what lets
    /// [`Self::resolve_sunlight`] drop the source guard before it
    /// touches the site's weather.
    sunlight_cache: AtomicU32,
    /// Active output: the command handed in through `set_command`,
    /// delayed and held inside the sun band. Seeded at the available
    /// power, so a fresh inverter is already generating.
    active: DeviceAxis,
    /// Reactive output, clamped to the capability at the last active
    /// output; telemetry reads its output.
    reactive: DeviceAxis,
    /// The live PF / kVA capability, changed at runtime.
    caps: Mutex<ReactiveCapability>,
}

/// What an array of `cfg.array_peak_w` produces at `pct` % sunlight
/// (negative), floored at the AC rating. Sunlight below 0 %, or NaN,
/// is no sun.
fn available_w(cfg: &SolarInverterConfig, pct: f32) -> f32 {
    let pct = pct.max(0.0);
    (-cfg.array_peak_w * pct / 100.0).max(cfg.rated_lower_w)
}

impl SolarInverter {
    pub fn new(id: u64, interval: Duration, cfg: SolarInverterConfig) -> Self {
        let source = match cfg.sunlight_pct {
            None => SunlightSource::Follow,
            Some(pct) => SunlightSource::manual(DynamicScalar::constant(pct)),
        };
        // Whatever the slot starts at: a `Follow` slot starts at the
        // cache's full-sun seed, a driven one at its constant.
        let init_pct = cfg.sunlight_pct.unwrap_or(SUNLIGHT_SEED_PCT);
        // A fresh PV inverter is already generating from whatever sun
        // it has — it does not slew up from zero. The same
        // `available_w` as every tick, so an oversized array
        // flat-tops at the AC rating from the very first sample.
        let active = DeviceAxis::new(cfg.device_delay, available_w(&cfg, init_pct));
        let reactive = DeviceAxis::new(cfg.device_delay, 0.0);
        Self {
            id,
            name: format!("inv-pv-{id}"),
            interval,
            caps: Mutex::new(cfg.reactive),
            cfg,
            sunlight_source: RwLock::new(source),
            sunlight_cache: AtomicU32::new(SUNLIGHT_SEED_PCT.to_bits()),
            active,
            reactive,
        }
    }

    /// Drop whatever drove the sunlight knob and go back to
    /// following the site's weather — the way back from
    /// `set_sunlight_pct` / `set_sunlight_source`, mirroring the
    /// meter's `clear_active_power_source`. The `Follow` it installs
    /// reads the configured lag and jitter at resolve time, so a
    /// cleared inverter tracks the sky exactly like a
    /// freshly-constructed weather-following one. Until the next tick
    /// resolves it, the knob reads whatever the cache last held.
    pub fn clear_sunlight(&self) {
        *self.sunlight_source.write() = SunlightSource::Follow;
    }

    /// The live percentage for `src`: `Manual`'s resolved scalar, or
    /// the value the last `tick` cached for `Follow`. Never blocks,
    /// never needs a site.
    fn pct_of(&self, src: &SunlightSource) -> f32 {
        match src {
            SunlightSource::Follow => f32::from_bits(self.sunlight_cache.load(Ordering::Acquire)),
            SunlightSource::Manual(scalar) => scalar.get(),
        }
    }

    pub fn sunlight_pct(&self) -> f32 {
        self.pct_of(&self.sunlight_source.read())
    }

    /// Resolve a `Follow` source against the site's weather and cache
    /// the result. No-op for `Manual`. Called from `tick`, the only
    /// door with both the site and `now`.
    ///
    /// Holds ONE lock at a time: the source guard is dropped before
    /// the site's weather lock is taken. There is no cross-lock hold
    /// here and so no lock-order invariant to respect — the cache is
    /// the inverter's own field, not something living inside the
    /// `Follow` variant, so nothing needs the slot pinned across the
    /// sample. A concurrent `set_sunlight_pct` may land between the
    /// check and the store; that is harmless, because a `Manual` slot
    /// never reads the cache, and a slot put back to `Follow`
    /// re-resolves on the next tick anyway.
    fn resolve_sunlight(&self, world: &MicrogridSite, now: DateTime<Utc>) {
        let following = matches!(&*self.sunlight_source.read(), SunlightSource::Follow);
        if !following {
            return;
        }
        let lag = self
            .cfg
            .weather_lag
            .unwrap_or_else(|| Self::default_weather_lag(self.id));
        let at = now - chrono::Duration::from_std(lag).unwrap_or_else(|_| chrono::Duration::zero());
        // No weather on the site at all → full sun, which is what a
        // weatherless site has always given a PV inverter.
        let mut pct = world.weather_pct_at(at).unwrap_or(100.0);
        if self.cfg.weather_jitter_pct != 0.0 {
            let j = self.cfg.weather_jitter_pct.abs();
            pct *= 1.0 + rand::thread_rng().gen_range(-j..=j) / 100.0;
        }
        self.sunlight_cache.store(pct.to_bits(), Ordering::Release);
    }

    /// The most the array can produce now (negative), floored at the
    /// AC rating.
    fn min_avail_w(&self) -> f32 {
        available_w(&self.cfg, self.sunlight_pct())
    }

    /// The band the sun allows now: from the available power
    /// (negative) up to the rated upper edge.
    fn sun_band(&self) -> VecBounds {
        VecBounds::single(self.min_avail_w(), self.cfg.rated_upper_w)
    }

    /// The Q band the capability allows at active power `p`.
    fn q_band_at(&self, p: f32) -> VecBounds {
        self.caps.lock().q_band_at(p)
    }

    /// The Follow lag used when no `:weather-lag-s` was given: a
    /// stable 0–60 s offset hashed from the component id, so a cloud
    /// sweeps across a multi-PV site by default. Hash, not RNG — the
    /// offset survives restarts and reloads with no stored state.
    /// SplitMix64's finalizer, because consecutive small ids (the
    /// common case) must land spread out, and a bare multiply mod 61
    /// leaves them clumped.
    fn default_weather_lag(id: u64) -> Duration {
        let mut z = id.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Duration::from_secs(z % 61)
    }
}

impl fmt::Display for SolarInverter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for SolarInverter {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Inverter
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        self.interval
    }
    fn refresh_inputs(&self, ctx: &mut TulispContext) {
        // Manual only — a `Follow` source has no Lisp to re-resolve,
        // it resolves against the site in `tick`. No-op for the
        // constant case too: DynamicScalar::refresh returns
        // immediately when there's no source expression.
        if let SunlightSource::Manual(scalar) = &*self.sunlight_source.read() {
            scalar.refresh(ctx);
        }
    }

    fn tick(&self, world: &MicrogridSite, now: DateTime<Utc>, _dt: Duration) {
        // Weather resolution happens BEFORE the health gate, on
        // purpose: the cached percentage is a reading of the SKY, not
        // of this inverter's output. A tripped inverter still sits
        // under the same clouds, and the inspector's sunlight knob
        // keeps showing the live sky rather than freezing at whatever
        // it was when the fault landed. Production is zeroed by the
        // gate below regardless of what the sun is doing.
        self.resolve_sunlight(world, now);
        // Own-health gate: a faulted or standby PV inverter is
        // tripped offline — zero output on both axes, nothing left in
        // either delay line. The gateway keeps the curtailment and
        // drops the Q command, so a recovered inverter resumes from
        // the sun.
        if world.runtime_of(self.id).health != Health::Ok {
            self.active.trip();
            self.reactive.trip();
            return;
        }
        // Q is judged at the previous tick's P, like the battery
        // inverter's.
        let p_prev = self.active.output();
        self.active.tick(now, Some(&self.sun_band()));
        self.reactive.tick(now, Some(&self.q_band_at(p_prev)));
    }

    fn telemetry(&self, site: &MicrogridSite) -> Telemetry {
        let p = self.active.output();
        super::inverter_telemetry(self.id, site, p, self.reactive.output())
    }

    fn active_power_w(&self, _site: &MicrogridSite) -> Option<f32> {
        Some(self.active.output())
    }

    fn aggregate_power_w(&self, _world: &MicrogridSite) -> f32 {
        self.active.output()
    }

    fn aggregate_reactive_var(&self, _world: &MicrogridSite) -> f32 {
        self.reactive.output()
    }

    fn controllable(&self) -> Option<&dyn Controllable> {
        Some(self)
    }

    fn reactive_limits(&self) -> Option<&dyn ReactiveLimits> {
        Some(self)
    }

    fn sunlight_drive(&self) -> Option<&dyn SunlightDrive> {
        Some(self)
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn subtype(&self) -> Option<&'static str> {
        Some("solar")
    }

    fn stream_jitter_pct(&self) -> f32 {
        self.cfg.stream_jitter_pct
    }

    fn snapshot_knob(&self, kind: KnobKind) -> Option<KnobSnapshot> {
        match kind {
            KnobKind::Sunlight => Some(KnobSnapshot::Sunlight(self.sunlight_source.read().clone())),
            _ => None,
        }
    }

    fn restore_knob(&self, snap: KnobSnapshot) -> bool {
        match snap {
            KnobSnapshot::Sunlight(source) => {
                *self.sunlight_source.write() = source;
                true
            }
            _ => false,
        }
    }

    fn make_fn(&self) -> &'static str {
        "%make-solar-inverter"
    }

    fn has_unrenderable_source(&self) -> bool {
        match &*self.sunlight_source.read() {
            // A `Follow` source is NEVER unrenderable: omitting
            // `:sunlight-pct` *is* its rendering, and a reloaded config
            // with no kwarg reconstructs a weather-following inverter
            // exactly. This arm deliberately ignores
            // `cfg.sunlight_dynamic`, which is sticky-true for the
            // life of an inverter built with a lambda — one that was
            // later cleared back to `Follow` no longer has any
            // expression to lose, so reporting it unrenderable would
            // block a save over a slot that renders perfectly.
            SunlightSource::Follow => false,
            // Three ways a driven slot outruns what can be written
            // back. The first two are dynamic sources, which have no
            // static number at all: constructed dynamic (which
            // `constructor_kwargs` already omits `:sunlight-pct` for)
            // and a runtime `(set-solar-sunlight ID (lambda …))`
            // poke, whose expression the generated block cannot carry
            // either — the same case Meter reports for
            // `set-meter-power`.
            //
            // The third is a plain CONSTANT poked over an inverter
            // built with no `:sunlight-pct` at all (`sunlight_pct` is
            // `None`). There is a number to write, but nowhere to
            // write it from: the renderer emits the constructed
            // kwarg, and this inverter never had one, so the poke
            // would be dropped silently and the saved file would come
            // back following the weather. Meter says the same thing
            // the same way, with `constructed_power.is_none() &&
            // power_source.is_some()`.
            SunlightSource::Manual(s) => {
                self.cfg.sunlight_dynamic || s.is_dynamic() || self.cfg.sunlight_pct.is_none()
            }
        }
    }

    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        let mut kw = super::common_inverter_kwargs(super::CommonInverterCfg {
            rated_lower_w: self.cfg.rated_lower_w,
            rated_upper_w: self.cfg.rated_upper_w,
            command_delay: self.cfg.command_delay,
            ramp_rate_w_per_s: self.cfg.ramp_rate_w_per_s,
            interval: self.interval,
            stream_jitter_pct: self.cfg.stream_jitter_pct,
            reactive: self.cfg.reactive,
            reactive_command_delay: self.cfg.reactive_command_delay,
            reactive_ramp_rate_var_per_s: self.cfg.reactive_ramp_rate_var_per_s,
            device_delay: self.cfg.device_delay,
        });
        // A dynamic sunlight source can't round-trip as a static
        // number — the renderer omits :sunlight-pct entirely rather
        // than writing the (possibly stale) fallback value. An
        // inverter built to follow the weather (`sunlight_pct: None`)
        // omits it too, but for the opposite reason: the absent kwarg
        // is precisely what a reload reads back as "this inverter
        // follows the weather". A slot driven to a constant at
        // runtime also omits it, since there is no constructed number
        // to write.
        let manual = matches!(&*self.sunlight_source.read(), SunlightSource::Manual(_));
        if let Some(pct) = self.cfg.sunlight_pct
            && !self.cfg.sunlight_dynamic
            && manual
        {
            kw.push((":sunlight-pct", crate::lisp::lisp_float32(pct)));
        }
        // Only write :array-peak-w when it diverges from the matched-array
        // default (|rated-lower|) — a matched config round-trips with
        // no extra noise.
        if (self.cfg.array_peak_w - self.cfg.rated_lower_w.abs()).abs() > f32::EPSILON {
            kw.push((
                ":array-peak-w",
                crate::lisp::lisp_float32(self.cfg.array_peak_w),
            ));
        }
        // The two `Follow` shaping kwargs. Lag is written whenever it
        // is explicit — zero included, since `:weather-lag-s 0` is
        // the opt-out from the id-derived default sweep and must
        // round-trip; an unset lag renders nothing, so a reloaded
        // inverter re-derives the same offset from its id. Jitter is
        // written only when it diverges from its zero default.
        if let Some(lag) = self.cfg.weather_lag {
            kw.push((
                ":weather-lag-s",
                crate::lisp::lisp_float32(lag.as_secs_f32()),
            ));
        }
        if self.cfg.weather_jitter_pct != 0.0 {
            kw.push((
                ":weather-jitter-pct",
                crate::lisp::lisp_float32(self.cfg.weather_jitter_pct),
            ));
        }
        kw
    }
}

impl SunlightDrive for SolarInverter {
    /// Drives the per-tick `min_avail = max(-array_peak_w ×
    /// sunlight_pct / 100, rated_lower_w)` clamp the inverter applies
    /// to incoming setpoints. Values are applied as-is; the per-tick
    /// clamp happens in `SolarInverter::min_avail_w`.
    fn set_sunlight_pct(&self, pct: f32) {
        *self.sunlight_source.write() = SunlightSource::manual(DynamicScalar::constant(pct));
    }

    /// Like every other driven value this installs a `Manual` source,
    /// displacing a `Follow` one if that is what was there.
    fn set_sunlight_source(&self, scalar: DynamicScalar) {
        *self.sunlight_source.write() = SunlightSource::manual(scalar);
    }

    fn clear_sunlight_source(&self) {
        self.clear_sunlight();
    }

    fn sunlight_reading(&self) -> ScalarReading {
        let s = self.sunlight_source.read();
        ScalarReading {
            value: self.pct_of(&s),
            // `Follow` has no Lisp source text, but it is not a plain
            // constant either — the inspector shows the "weather"
            // marker in the same slot a lambda's printed form goes,
            // so a reader can tell a tracked sky from a driven
            // number.
            expr: match &*s {
                SunlightSource::Follow => Some("weather".into()),
                SunlightSource::Manual(scalar) => scalar.source_text(),
            },
        }
    }
}

impl ReactiveLimits for SolarInverter {
    fn reactive_capability(&self) -> ReactiveCapability {
        *self.caps.lock()
    }

    fn set_reactive_pf_limit(&self, pf: Option<f32>) {
        self.caps.lock().pf_limit = pf;
    }

    fn set_reactive_apparent_va(&self, va: Option<f32>) {
        self.caps.lock().apparent_va = va;
    }
}

impl Controllable for SolarInverter {
    fn has_axis(&self, _axis: SetpointAxis) -> bool {
        true
    }

    fn set_command(&self, axis: SetpointAxis, value: f32) {
        match axis {
            SetpointAxis::Active => self.active.set_command(value),
            SetpointAxis::Reactive => self.reactive.set_command(value),
        }
    }

    /// P: the sun band (not advertised and not checked by an
    /// augmentation — a curtailment must be accepted at night). Q:
    /// the caps at the last active output.
    fn physical_band(&self, axis: SetpointAxis, _dt: Duration) -> Option<VecBounds> {
        Some(match axis {
            SetpointAxis::Active => self.sun_band(),
            SetpointAxis::Reactive => self.q_band_at(self.active.output()),
        })
    }

    /// Free-running PV tracks the sun, and an expired or reset
    /// curtailment releases to the sunlight floor; Q holds.
    fn idle_value(&self, axis: SetpointAxis) -> Option<f32> {
        (axis == SetpointAxis::Active).then(|| self.min_avail_w())
    }

    fn initial_value(&self, axis: SetpointAxis) -> f32 {
        match axis {
            SetpointAxis::Active => self.active.output(),
            SetpointAxis::Reactive => 0.0,
        }
    }

    /// A curtailment survives a trip; a Q command does not.
    fn keeps_command_through_fault(&self, axis: SetpointAxis) -> bool {
        axis == SetpointAxis::Active
    }

    fn gateway_settings(&self) -> GatewaySettings {
        GatewaySettings {
            command_delay: self.cfg.command_delay,
            ramp_rate_w_per_s: self.cfg.ramp_rate_w_per_s,
            reactive_command_delay: self.cfg.reactive_command_delay,
            reactive_ramp_rate_var_per_s: self.cfg.reactive_ramp_rate_var_per_s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeout_tracker::SetpointAxis;

    const DT: Duration = Duration::from_millis(100);

    fn cfg_with_sun(pct: f32) -> SolarInverterConfig {
        SolarInverterConfig {
            rated_lower_w: -10_000.0,
            rated_upper_w: 0.0,
            sunlight_pct: Some(pct),
            ramp_rate_w_per_s: f32::INFINITY,
            array_peak_w: 10_000.0,
            device_delay: Duration::ZERO,
            ..Default::default()
        }
    }

    #[test]
    fn sunlight_pct_drives_min_avail_floor() {
        let inv = SolarInverter::new(1, Duration::from_secs(1), cfg_with_sun(50.0));
        // 50% of -10 kW rated = -5 kW available.
        assert!((inv.min_avail_w() - (-5_000.0)).abs() < 1e-3);

        // Sun goes behind a cloud → less generation available.
        inv.set_sunlight_pct(20.0);
        assert!((inv.min_avail_w() - (-2_000.0)).abs() < 1e-3);

        // Overdrive now clamps at the AC rating rather than
        // overdriving past it — the microsim's out-of-range
        // pass-through is retired. Expressing overdrive intent is
        // now `:array-peak-w`'s job (see `oversized_array_clips_at_the_ac_rating`).
        inv.set_sunlight_pct(150.0);
        assert!((inv.min_avail_w() - (-10_000.0)).abs() < 1e-3);
    }

    /// An oversized DC array flat-tops at the inverter's AC rating;
    /// a matched array is unchanged from the pre-:array-peak-w behavior.
    #[test]
    fn oversized_array_clips_at_the_ac_rating() {
        let matched = SolarInverter::new(1, Duration::from_secs(1), SolarInverterConfig::default());
        assert!((matched.min_avail_w() - (-30_000.0)).abs() < 1e-3);
        let oversized = SolarInverter::new(
            2,
            Duration::from_secs(1),
            SolarInverterConfig {
                array_peak_w: 45_000.0,
                ..Default::default()
            },
        );
        // 100% sun: 45 kW of array clamped to the 30 kW rating.
        assert!((oversized.min_avail_w() - (-30_000.0)).abs() < 1e-3);
        // 50% sun: 22.5 kW — inside the rating, no clamp.
        oversized.set_sunlight_pct(50.0);
        assert!((oversized.min_avail_w() - (-22_500.0)).abs() < 1e-3);
    }

    /// Sunlight below 0 %, or NaN, means no sun, not a sun that draws
    /// power: the available output stays at 0 rather than going
    /// positive, from the first sample on.
    #[test]
    fn negative_or_nan_sunlight_gives_no_output() {
        let w = MicrogridSite::new();
        for pct in [-20.0, f32::NAN] {
            let inv = SolarInverter::new(1, Duration::from_secs(1), cfg_with_sun(pct));
            assert_eq!(inv.min_avail_w(), 0.0, "sunlight {pct}");
            assert_eq!(
                inv.telemetry(&w).active_power_w,
                Some(0.0),
                "first sample at sunlight {pct}"
            );
        }
    }

    /// +inf sunlight clips at the AC rating, as any sunlight does
    /// once the array's output reaches it.
    #[test]
    fn infinite_sunlight_clips_at_the_ac_rating() {
        let inv = SolarInverter::new(1, Duration::from_secs(1), cfg_with_sun(f32::INFINITY));
        assert_eq!(inv.min_avail_w(), -10_000.0);
    }

    /// The initial output snapped at construction uses the same
    /// array-clamp shape as `min_avail_w` — an oversized array
    /// flat-tops at the AC rating from the very first sample, BEFORE
    /// any tick has run, not just from the first tick onward.
    #[test]
    fn initial_output_clamps_at_the_ac_rating_before_any_tick() {
        let w = MicrogridSite::new();
        let cfg = SolarInverterConfig {
            rated_lower_w: -30_000.0,
            rated_upper_w: 0.0,
            sunlight_pct: Some(80.0),
            array_peak_w: 45_000.0,
            ramp_rate_w_per_s: f32::INFINITY,
            ..Default::default()
        };
        let inv = SolarInverter::new(1, Duration::from_secs(1), cfg);
        // 80% of the 45 kW array is -36 kW, clamped to the -30 kW AC
        // rating — NOT -24,000 W, what the pre-array-clamp formula
        // (rated_lower_w × init_pct / 100) would have snapped to.
        let p = inv
            .telemetry(&w)
            .active_power_w
            .expect("active power present");
        assert!(
            (p - (-30_000.0)).abs() < 1e-3,
            "expected AC-clamped -30000 W before any tick, got {p}"
        );
    }

    /// A dynamic sunlight source resolves on each `refresh_inputs`,
    /// driving the min_avail floor without going through the
    /// imperative `(set-solar-sunlight)` setter.
    #[test]
    fn dynamic_sunlight_source_refreshes() {
        let mut ctx = tulisp::TulispContext::new();
        let inv = SolarInverter::new(1, Duration::from_secs(1), cfg_with_sun(100.0));
        let lambda = ctx.eval_string("(lambda () 40.0)").unwrap();
        let scalar = DynamicScalar::from_lisp(&lambda, 100.0).expect("lambda → dynamic");
        inv.set_sunlight_source(scalar);

        // Pre-refresh: the cached fallback (100.0) is still in effect.
        assert!((inv.min_avail_w() - (-10_000.0)).abs() < 1e-3);

        // Refresh resolves the lambda → 40% of -10 kW rated = -4 kW.
        inv.refresh_inputs(&mut ctx);
        assert!((inv.min_avail_w() - (-4_000.0)).abs() < 1e-3);
    }

    /// `set_sunlight_pct` collapses any prior dynamic source, so a
    /// timer- or scenario-driven imperative override wins over a
    /// configured lambda.
    #[test]
    fn set_sunlight_pct_collapses_dynamic_source() {
        let mut ctx = tulisp::TulispContext::new();
        let inv = SolarInverter::new(1, Duration::from_secs(1), cfg_with_sun(100.0));
        let lambda = ctx.eval_string("(lambda () 70.0)").unwrap();
        inv.set_sunlight_source(DynamicScalar::from_lisp(&lambda, 100.0).unwrap());
        inv.refresh_inputs(&mut ctx);
        assert!((inv.min_avail_w() - (-7_000.0)).abs() < 1e-3);

        inv.set_sunlight_pct(30.0);
        // Subsequent refresh is a no-op on the constant.
        inv.refresh_inputs(&mut ctx);
        assert!((inv.min_avail_w() - (-3_000.0)).abs() < 1e-3);
    }

    /// A faulted PV inverter trips offline: it produces nothing rather
    /// than falling back to full sunlight-tracking. On recovery it
    /// reconnects and resumes producing from the available sunlight.
    #[test]
    fn errored_inverter_stops_producing() {
        let w = MicrogridSite::new();
        let inv = SolarInverter::new(1, Duration::from_secs(1), cfg_with_sun(100.0));
        w.register(inv);
        let inv = w.get(1).unwrap();
        let dt = Duration::from_millis(100);

        // Healthy at full sun: produces its rated -10 kW.
        w.tick_n(1, dt);
        assert!((inv.aggregate_power_w(&w) - (-10_000.0)).abs() < 1.0);

        // Errored: tripped offline, zero output — NOT sunlight production.
        w.set_health(1, Health::Error).unwrap();
        w.tick_n(1, dt);
        assert!(
            inv.aggregate_power_w(&w).abs() < 1.0,
            "errored PV inverter must produce 0 W, got {}",
            inv.aggregate_power_w(&w),
        );

        // Recovery: a PV inverter reconnects and resumes from sunlight.
        w.set_health(1, Health::Ok).unwrap();
        w.tick_n(1, dt);
        assert!((inv.aggregate_power_w(&w) - (-10_000.0)).abs() < 1.0);
    }

    /// A health trip kills the reactive axis outright — Q snaps to 0
    /// and its command is cleared — while the active curtailment
    /// survives and P resumes at it on recovery.
    #[test]
    fn health_trip_trips_q_but_keeps_the_armed_curtailment() {
        let w = MicrogridSite::new();
        let mut cfg = cfg_with_sun(100.0);
        cfg.reactive = ReactiveCapability {
            pf_limit: None,
            apparent_va: Some(10_000.0),
        };
        cfg.reactive_command_delay = Duration::ZERO;
        cfg.reactive_ramp_rate_var_per_s = f32::INFINITY;
        w.register(SolarInverter::new(1, Duration::from_secs(1), cfg));
        let inv = w.get(1).unwrap();
        let gw = w.gateway();

        gw.command(1, SetpointAxis::Active, -4_000.0).unwrap();
        w.tick_n(1, DT);
        gw.command(1, SetpointAxis::Reactive, 2_000.0).unwrap();
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - (-4_000.0)).abs() < 1.0);
        assert!((inv.aggregate_reactive_var(&w) - 2_000.0).abs() < 1.0);

        w.set_health(1, Health::Error).unwrap();
        w.tick_n(1, DT);
        assert!(inv.aggregate_power_w(&w).abs() < 1.0, "P snaps to 0");
        assert!(inv.aggregate_reactive_var(&w).abs() < 1.0, "Q snaps to 0");

        w.set_health(1, Health::Ok).unwrap();
        w.tick_n(1, DT);
        assert!(
            (inv.aggregate_power_w(&w) - (-4_000.0)).abs() < 1.0,
            "P resumes at the curtailment, got {}",
            inv.aggregate_power_w(&w),
        );
        assert!(
            inv.aggregate_reactive_var(&w).abs() < 1.0,
            "Q awaits re-dispatch"
        );

        gw.command(1, SetpointAxis::Reactive, 1_500.0).unwrap();
        w.tick_n(1, DT);
        assert!((inv.aggregate_reactive_var(&w) - 1_500.0).abs() < 1.0);
    }

    /// The trip keeps the curtailment's lifetime as well as the
    /// command, and drops the reactive lifetime with the Q command.
    #[test]
    fn a_trip_keeps_the_curtailment_and_its_lifetime_but_drops_q() {
        let w = MicrogridSite::new();
        let mut cfg = cfg_with_sun(100.0);
        cfg.reactive = ReactiveCapability {
            pf_limit: None,
            apparent_va: Some(10_000.0),
        };
        cfg.reactive_command_delay = Duration::ZERO;
        w.register(SolarInverter::new(1, Duration::from_secs(1), cfg));
        let gw = w.gateway();
        gw.command(1, SetpointAxis::Active, -4_000.0).unwrap();
        w.tick_n(1, DT);
        gw.command(1, SetpointAxis::Reactive, 2_000.0).unwrap();
        w.tick_n(1, DT);
        w.set_health(1, Health::Error).unwrap();
        w.tick_n(2, DT);
        assert!(gw.remaining_lifetime(1, SetpointAxis::Active).is_some());
        assert_eq!(gw.remaining_lifetime(1, SetpointAxis::Reactive), None);
        w.set_health(1, Health::Ok).unwrap();
        w.tick_n(1, DT);
        assert!((w.get(1).unwrap().aggregate_power_w(&w) - (-4_000.0)).abs() < 1.0);
    }

    /// A reset releases a curtailment to the SUNLIGHT FLOOR, not to
    /// zero; an active-axis reset leaves a Q command running.
    #[test]
    fn reset_releases_a_curtailment_back_to_the_sunlight_floor() {
        let w = MicrogridSite::new();
        let mut cfg = cfg_with_sun(60.0);
        cfg.reactive = ReactiveCapability {
            pf_limit: None,
            apparent_va: Some(10_000.0),
        };
        cfg.reactive_command_delay = Duration::ZERO;
        cfg.reactive_ramp_rate_var_per_s = f32::INFINITY;
        w.register(SolarInverter::new(1, Duration::from_secs(1), cfg));
        let inv = w.get(1).unwrap();
        let gw = w.gateway();
        let floor = -6_000.0;

        gw.command(1, SetpointAxis::Active, -2_000.0).unwrap();
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - (-2_000.0)).abs() < 1.0);

        gw.reset(1, SetpointAxis::Active);
        gw.reset(1, SetpointAxis::Reactive);
        w.tick_n(1, DT);
        assert!(
            (inv.aggregate_power_w(&w) - floor).abs() < 1.0,
            "reset must return to the sunlight floor {floor}, got {}",
            inv.aggregate_power_w(&w),
        );

        gw.command(1, SetpointAxis::Active, -1_000.0).unwrap();
        gw.command(1, SetpointAxis::Reactive, 3_000.0).unwrap();
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - (-1_000.0)).abs() < 1.0);
        assert!((inv.aggregate_reactive_var(&w) - 3_000.0).abs() < 1.0);

        gw.reset(1, SetpointAxis::Active);
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - floor).abs() < 1.0);
        assert!(
            (inv.aggregate_reactive_var(&w) - 3_000.0).abs() < 1.0,
            "Q survives"
        );
    }

    /// An augmentation demanding more production than the sun allows
    /// leaves no legal band: the output parks at 0. The augmentation
    /// itself is accepted (a curtailment must be accepted at night).
    #[test]
    fn augmentation_beyond_available_sun_parks_at_zero() {
        let w = MicrogridSite::new();
        let cfg = SolarInverterConfig {
            rated_lower_w: -30_000.0,
            rated_upper_w: 0.0,
            sunlight_pct: Some(10.0),
            ramp_rate_w_per_s: f32::INFINITY,
            device_delay: Duration::ZERO,
            ..Default::default()
        };
        w.register(SolarInverter::new(1, Duration::from_secs(1), cfg));
        w.gateway()
            .augment(
                1,
                w.run_generation(),
                SetpointAxis::Active,
                VecBounds::single(-8_000.0, -6_000.0),
                Duration::from_secs(60),
            )
            .unwrap();
        w.tick_n(1, DT);
        let p = w.get(1).unwrap().aggregate_power_w(&w);
        assert!(p.abs() < 1.0, "no legal band left → park at 0, got {p}");
    }

    /// A fresh inverter starts at its available sun and does not slew
    /// up from zero, even with a ramp.
    #[test]
    fn a_fresh_inverter_does_not_slew_up_from_zero() {
        let w = MicrogridSite::new();
        w.register(SolarInverter::new(
            1,
            Duration::from_secs(1),
            SolarInverterConfig {
                ramp_rate_w_per_s: 2_000.0,
                ..cfg_with_sun(60.0)
            },
        ));
        w.tick_n(1, DT);
        assert!((w.get(1).unwrap().aggregate_power_w(&w) - (-6_000.0)).abs() < 1.0);
    }

    /// A static `:sunlight-pct` renders as its own kwarg, sharing the
    /// same rated / command-delay / reactive kwargs as the battery
    /// inverter.
    #[test]
    fn constructor_kwargs_round_trip_solar() {
        let mut cfg = cfg_with_sun(42.0);
        cfg.rated_lower_w = -12_000.0;
        let inv = SolarInverter::new(5, Duration::from_secs(1), cfg);
        assert_eq!(inv.make_fn(), "%make-solar-inverter");
        let s = inv
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(s.contains(":sunlight-pct 42.0"));
        assert!(s.contains(":rated-lower-w -12000.0"));
    }

    /// A lambda- or symbol-driven sunlight source can't round-trip
    /// as a static number, so `:sunlight-pct` is omitted entirely.
    #[test]
    fn constructor_kwargs_omits_sunlight_pct_when_dynamic() {
        let mut cfg = cfg_with_sun(100.0);
        cfg.sunlight_dynamic = true;
        let inv = SolarInverter::new(6, Duration::from_secs(1), cfg);
        let s = inv
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!s.contains(":sunlight-pct"));
        assert!(inv.has_unrenderable_source());
    }

    /// A sunlight source installed at RUNTIME — a scenario or an
    /// `every` block calling `(set-solar-sunlight ID (lambda …))` —
    /// is just as unwritable as a constructed one, so the component
    /// has to report it. The construction-time flag alone missed it,
    /// which is the same gap Meter closes by consulting its live
    /// power source.
    #[test]
    fn a_runtime_lambda_sunlight_poke_reports_unrenderable() {
        let inv = SolarInverter::new(6, Duration::from_secs(1), cfg_with_sun(80.0));
        assert!(!inv.has_unrenderable_source(), "a static one is fine");
        let mut ctx = tulisp::TulispContext::new();
        let lambda = ctx.eval_string("(lambda () 40.0)").unwrap();
        inv.set_sunlight_source(DynamicScalar::from_lisp(&lambda, 80.0).unwrap());
        assert!(inv.has_unrenderable_source());
    }

    /// The other unwritable poke, and the one a lambda check alone
    /// misses: a plain CONSTANT driven over an inverter built with no
    /// `:sunlight-pct` at all. The number is perfectly renderable in the
    /// abstract, but this inverter has no constructed `:sunlight-pct`
    /// for the renderer to write it into — `constructor_kwargs` omits
    /// the kwarg, so a save would drop the poke and reload as a
    /// weather-following inverter. Reporting it unrenderable is what
    /// makes that loss visible instead of silent, exactly as Meter
    /// does for a `set-meter-power` over a meter built without
    /// `:power-w`.
    #[test]
    fn a_constant_poked_over_a_follow_built_inverter_reports_unrenderable() {
        let inv = SolarInverter::new(
            7,
            Duration::from_secs(1),
            SolarInverterConfig {
                sunlight_pct: None,
                ..Default::default()
            },
        );
        assert!(
            !inv.has_unrenderable_source(),
            "following the weather renders by omission",
        );

        inv.set_sunlight_pct(30.0);
        assert!(
            inv.has_unrenderable_source(),
            "a constant with no constructed kwarg to carry it",
        );
        let s = inv
            .constructor_kwargs()
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !s.contains(":sunlight-pct"),
            "…and the renderer really has nowhere to put it, got {s}",
        );
    }

    /// `cfg.sunlight_dynamic` is sticky — it records how the inverter
    /// was BUILT and never clears. Clearing the slot back to `Follow`
    /// leaves no expression to lose, so the inverter is renderable
    /// again: the omitted `:sunlight-pct` is exactly how a Follow slot
    /// is written. Consulting the stale flag on a Follow source would
    /// wrongly mark a perfectly renderable microgrid unsaveable.
    #[test]
    fn a_cleared_follow_slot_is_renderable_despite_the_sticky_dynamic_flag() {
        let mut cfg = cfg_with_sun(100.0);
        cfg.sunlight_dynamic = true;
        let inv = SolarInverter::new(6, Duration::from_secs(1), cfg);
        assert!(inv.has_unrenderable_source(), "built with a lambda");

        inv.clear_sunlight();
        assert!(
            !inv.has_unrenderable_source(),
            "a Follow slot renders by omission",
        );
        let s = inv
            .constructor_kwargs()
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!s.contains(":sunlight-pct"), "and the kwarg stays omitted");
    }

    /// Snapshot/restore round-trip for the sunlight knob: a dynamic
    /// (lambda) source survives being swapped out for a constant and
    /// back — `is_dynamic()` and the printed source text both come
    /// back exactly as they were, because `restore_knob` writes the
    /// captured `SunlightSource` (and the `DynamicScalar` inside it)
    /// itself, not a re-parse of its text.
    #[test]
    fn snapshot_restore_round_trip_sunlight_dynamic_source() {
        let mut ctx = tulisp::TulispContext::new();
        let inv = SolarInverter::new(1, Duration::from_secs(1), cfg_with_sun(50.0));
        let lambda = ctx.eval_string("(lambda () 33.0)").unwrap();
        let scalar = DynamicScalar::from_lisp(&lambda, 50.0).unwrap();
        inv.set_sunlight_source(scalar);
        let text_before = inv.sunlight_reading().expr;
        assert!(text_before.is_some());

        let snap = inv.snapshot_knob(KnobKind::Sunlight).unwrap();

        // A scenario collapses it to a constant.
        inv.set_sunlight_pct(10.0);
        assert!(inv.sunlight_reading().expr.is_none());
        assert!(!inv.has_unrenderable_source());

        assert!(inv.restore_knob(snap));
        assert_eq!(inv.sunlight_reading().expr, text_before);
        assert!(inv.has_unrenderable_source(), "dynamic source restored");
    }

    /// A Follow-source inverter tracks site weather at now − lag and
    /// falls back to 100% when the site has no weather.
    ///
    /// Order matters here. The cache is *seeded* at 100, so asserting
    /// the weatherless fallback on a fresh inverter would pass whether
    /// or not `resolve_sunlight` ran at all — and so would asserting
    /// it right after any tick that legitimately resolved to 100. The
    /// weatherless check therefore comes LAST, after a 09:30 tick has
    /// driven the cache down to ≈70.7, and re-ticks at that same 09:30
    /// so the removal of the weather is the only variable in play.
    #[test]
    fn follow_source_tracks_site_weather() {
        use crate::sim::weather::{Weather, WeatherConfig};
        use chrono::TimeZone;
        let w = MicrogridSite::new();
        let inv = SolarInverter::new(
            1,
            Duration::from_secs(1),
            SolarInverterConfig {
                sunlight_pct: None,
                // Opt out of the id-derived default lag: the exact
                // 09:30 assertion below needs an unshifted sample.
                weather_lag: Some(Duration::ZERO),
                ..Default::default()
            },
        );
        w.register(inv);
        let inv = w.get(1).unwrap();
        let dt = Duration::from_millis(100);
        let morning = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 9, 30, 0).unwrap();
        let noon = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 13, 0, 0).unwrap();

        // With weather, at solar noon: the clear-sky peak.
        w.set_weather(Some(Weather::new(WeatherConfig::default())));
        w.tick_once(noon, dt);
        let r = inv.sunlight_drive().unwrap().sunlight_reading();
        assert!(
            (r.value - 100.0).abs() < 0.01,
            "solar noon → 100, got {}",
            r.value,
        );
        assert_eq!(
            r.expr.as_deref(),
            Some("weather"),
            "Follow read-back marker"
        );

        // At 09:30 — 3.5 h into a 14 h day: sin(π/4)·100. This tick is
        // also what drives the cache OFF 100, giving the fallback
        // check below something to move back from.
        w.tick_once(morning, dt);
        let pct = inv.sunlight_drive().unwrap().sunlight_reading().value;
        let expect = 100.0 * (std::f32::consts::PI * 0.25).sin();
        assert!((pct - expect).abs() < 0.1, "expected {expect}, got {pct}");

        // Weather removed, ticking at the SAME 09:30: the only thing
        // that changed is the site's weather, so a reading back at 100
        // can only have come from `unwrap_or(100.0)` actually running.
        // Skip the fallback and the cache would still read ≈70.7.
        w.set_weather(None);
        w.tick_once(morning, dt);
        let pct = inv.sunlight_drive().unwrap().sunlight_reading().value;
        assert!((pct - 100.0).abs() < 0.01, "no weather → 100, got {pct}");
    }

    /// Setting the knob overrides weather with a Manual constant;
    /// clearing returns to Follow. The step between the two is the
    /// cache contract the clear door documents: `clear_sunlight`
    /// leaves the cache alone, so the knob reads the last sky the
    /// tick resolved — here the night's 0 — from the instant of the
    /// clear, not the 42 it was just driven at and not the
    /// never-ticked full-sun seed. A clear that reset the cache, or
    /// one that left the Manual value showing, fails on that reading
    /// rather than surviving to the next tick where both spellings
    /// converge on 0.
    #[test]
    fn manual_override_and_clear_round_trip() {
        use crate::sim::weather::{Weather, WeatherConfig};
        use chrono::TimeZone;
        let w = MicrogridSite::new();
        w.register(SolarInverter::new(
            1,
            Duration::from_secs(1),
            SolarInverterConfig {
                sunlight_pct: None,
                ..Default::default()
            },
        ));
        w.set_weather(Some(Weather::new(WeatherConfig::default())));
        let inv = w.get(1).unwrap();
        let night = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 2, 0, 0).unwrap();

        // One Follow tick first, so the cache holds a genuinely
        // RESOLVED sky (the night's 0) rather than the construction
        // seed — otherwise the reading after the clear below proves
        // nothing about what the cache last held.
        w.tick_once(night, Duration::from_millis(100));
        assert_eq!(
            inv.sunlight_drive().unwrap().sunlight_reading().value,
            0.0,
            "the Follow tick resolves the night sky into the cache"
        );

        inv.sunlight_drive().unwrap().set_sunlight_pct(42.0);
        w.tick_once(night, Duration::from_millis(100));
        assert!((inv.sunlight_drive().unwrap().sunlight_reading().value - 42.0).abs() < 0.01);
        inv.sunlight_drive().unwrap().clear_sunlight_source();
        assert_eq!(
            inv.sunlight_drive().unwrap().sunlight_reading().value,
            0.0,
            "the clear reads the last resolved sky at once, before any tick"
        );
        w.tick_once(night, Duration::from_millis(100));
        assert_eq!(
            inv.sunlight_drive().unwrap().sunlight_reading().value,
            0.0,
            "night sky via Follow"
        );
    }

    /// `weather_lag` really shifts the sample back in time: a Follow
    /// inverter with an hour of lag, ticked at solar noon, reads the
    /// sky as it was an HOUR AGO. The default 06:00–20:00 day makes
    /// the two readings 100 and 100·sin(π·6/14) ≈ 97.49 — close
    /// enough to be plausible physics, far enough apart that a lag
    /// silently dropped on the floor fails here instead of reverting
    /// clean.
    #[test]
    fn follow_source_reads_the_sky_one_lag_ago() {
        use crate::sim::weather::{Weather, WeatherConfig};
        use chrono::TimeZone;
        let w = MicrogridSite::new();
        w.register(SolarInverter::new(
            1,
            Duration::from_secs(1),
            SolarInverterConfig {
                sunlight_pct: None,
                weather_lag: Some(Duration::from_secs(3_600)),
                ..Default::default()
            },
        ));
        w.set_weather(Some(Weather::new(WeatherConfig::default())));
        let inv = w.get(1).unwrap();
        let noon = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 13, 0, 0).unwrap();

        w.tick_once(noon, Duration::from_millis(100));
        let pct = inv.sunlight_drive().unwrap().sunlight_reading().value;
        let an_hour_ago = 100.0 * (std::f32::consts::PI * 6.0 / 14.0).sin();
        assert!(
            (pct - an_hour_ago).abs() < 0.05,
            "an hour of lag reads 12:00's {an_hour_ago}, got {pct}"
        );
        assert!(
            (pct - 100.0).abs() > 1.0,
            "…and that is distinguishable from the unlagged 100 at noon"
        );
    }

    /// An unset `:weather-lag-s` gives each inverter a stable
    /// id-derived offset (0–60 s) so a cloud sweeps across a multi-PV
    /// site by default; an explicit zero opts out. Pinned with a
    /// hard-edged cloud: the lagged inverter still reads the
    /// pre-cloud sky while the opted-out one reads the cloud.
    #[test]
    fn unset_lag_derives_a_stable_offset_and_zero_opts_out() {
        use crate::sim::weather::{Weather, WeatherConfig};
        use chrono::TimeZone;
        let lag = SolarInverter::default_weather_lag;
        assert_eq!(lag(1581), lag(1581), "stable across calls");
        // The hash must SPREAD consecutive ids — the whole point of
        // the SplitMix64 finalizer. A weak mixer (`id % 61`) gives a
        // mean neighbor gap of ~1 s and would pass any existence
        // check; SplitMix64 lands near 20 s.
        let mean_gap: f64 = (1..50u64)
            .map(|i| (lag(i).as_secs() as f64 - lag(i + 1).as_secs() as f64).abs())
            .sum::<f64>()
            / 49.0;
        assert!(
            mean_gap > 15.0,
            "consecutive ids must spread, mean neighbor gap {mean_gap:.1} s"
        );
        // Pick an id whose derived lag clears the 30 s cloud age
        // below, so nothing here depends on the hash constant.
        let lagged_id = (1..200u64)
            .find(|i| lag(*i).as_secs() > 45)
            .expect("some id under 200 hashes past 45 s");

        let w = MicrogridSite::new();
        w.register(SolarInverter::new(
            lagged_id,
            Duration::from_secs(1),
            SolarInverterConfig {
                sunlight_pct: None,
                ..Default::default()
            },
        ));
        w.register(SolarInverter::new(
            lagged_id + 200,
            Duration::from_secs(1),
            SolarInverterConfig {
                sunlight_pct: None,
                weather_lag: Some(Duration::ZERO),
                ..Default::default()
            },
        ));
        w.set_weather(Some(Weather::new(WeatherConfig::default())));
        let noon = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 13, 0, 0).unwrap();
        let dt = Duration::from_millis(100);
        // Anchor the weather 30 s before noon, then drop a hard-edged
        // full cloud there: at noon the sky reads 0, but more than
        // 45 s ago it was still clear.
        w.tick_once(noon - chrono::Duration::seconds(30), dt);
        w.with_weather(|wx| wx.pass_cloud(100.0, Duration::from_secs(600), Duration::ZERO));
        w.tick_once(noon, dt);
        let lagged = w
            .get(lagged_id)
            .unwrap()
            .sunlight_drive()
            .unwrap()
            .sunlight_reading()
            .value;
        let instant = w
            .get(lagged_id + 200)
            .unwrap()
            .sunlight_drive()
            .unwrap()
            .sunlight_reading()
            .value;
        assert!(
            instant < 1.0,
            "explicit zero lag sees the cloud, got {instant}"
        );
        assert!(
            (lagged - 100.0).abs() < 0.5,
            "the derived lag still reads the pre-cloud sky, got {lagged}"
        );
    }

    /// `weather_jitter_pct` roughens each tick's sample: successive
    /// ticks at the SAME instant (so the sky itself cannot be what
    /// moved) must differ, and every one must stay inside the ±50%
    /// band around the 100% base. A jitter dropped on the floor
    /// fails the first half; one applied unbounded fails the second.
    #[test]
    fn follow_source_jitters_within_the_configured_band() {
        use crate::sim::weather::{Weather, WeatherConfig};
        use chrono::TimeZone;
        let w = MicrogridSite::new();
        w.register(SolarInverter::new(
            1,
            Duration::from_secs(1),
            SolarInverterConfig {
                sunlight_pct: None,
                weather_jitter_pct: 50.0,
                ..Default::default()
            },
        ));
        w.set_weather(Some(Weather::new(WeatherConfig::default())));
        let inv = w.get(1).unwrap();
        let noon = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 13, 0, 0).unwrap();

        let mut seen = Vec::new();
        for _ in 0..12 {
            w.tick_once(noon, Duration::from_millis(100));
            seen.push(inv.sunlight_drive().unwrap().sunlight_reading().value);
        }
        assert!(
            seen.windows(2).any(|p| (p[0] - p[1]).abs() > f32::EPSILON),
            "jitter must move the sample tick to tick, got {seen:?}"
        );
        for v in &seen {
            assert!(
                (50.0..=150.0).contains(v),
                "±50% of the 100% base is [50, 150], got {v} in {seen:?}"
            );
        }
    }

    /// The knob snapshot round-trips a Follow source: a scenario that
    /// drives sunlight restores back to Follow, not a stale constant.
    #[test]
    fn knob_snapshot_restores_follow() {
        let inv = SolarInverter::new(
            1,
            Duration::from_secs(1),
            SolarInverterConfig {
                sunlight_pct: None,
                ..Default::default()
            },
        );
        let snap = inv.snapshot_knob(KnobKind::Sunlight).unwrap();
        inv.set_sunlight_pct(5.0);
        assert!(inv.restore_knob(snap));
        assert!(matches!(
            &*inv.sunlight_source.read(),
            SunlightSource::Follow
        ));
    }

    /// The other direction, which the Follow test alone can't cover:
    /// a scenario opening on a MANUALLY driven inverter must restore
    /// the driven constant, not leave it following the weather. The
    /// two together pin the payload as carrying the whole source —
    /// a restore that hard-coded either variant would fail one of
    /// them.
    #[test]
    fn knob_snapshot_restores_manual_over_follow() {
        let inv = SolarInverter::new(
            1,
            Duration::from_secs(1),
            SolarInverterConfig {
                sunlight_pct: None,
                ..Default::default()
            },
        );
        // Something drove the knob to 42 before the scenario started.
        inv.set_sunlight_pct(42.0);
        let snap = inv.snapshot_knob(KnobKind::Sunlight).unwrap();

        // The scenario clears it back to weather-following...
        inv.clear_sunlight();
        assert_eq!(inv.sunlight_reading().expr.as_deref(), Some("weather"));

        // ... and teardown puts the driven constant back.
        assert!(inv.restore_knob(snap));
        let r = inv.sunlight_reading();
        assert!(
            (r.value - 42.0).abs() < 0.01,
            "restored 42, got {}",
            r.value
        );
        assert_eq!(r.expr, None, "a restored constant carries no marker");
        assert!(matches!(
            &*inv.sunlight_source.read(),
            SunlightSource::Manual(_)
        ));
    }
}
