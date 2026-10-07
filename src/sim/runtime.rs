//! Per-component runtime knobs that simulate device-side faults
//! independent of the physics layer.
//!
//! These three orthogonal flags let a config (or a runtime caller via
//! `(set-component-* …)` defuns) drive faulty behaviour without
//! touching the simulated state. They live in the MicrogridSite, not on the
//! component, because the things they control (gRPC stream pacing,
//! request handling) are server-facing concerns the components
//! shouldn't know about.

tulisp::AsSymbol! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub enum Health {
        /// Component reports the physics-derived state code (ready /
        /// charging / discharging / …) and accepts setpoints normally.
        #[default]
        Ok<"ok">,
        /// Component reports `ERROR`; setpoint requests are rejected at
        /// the gRPC layer with `FailedPrecondition`.
        Error<"error">,
        /// Component reports `STANDBY`; setpoint requests are rejected.
        Standby<"standby">,
    }
}

tulisp::AsSymbol! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub enum TelemetryMode {
        /// Emit samples at the component's stream interval (the default).
        #[default]
        Normal<"normal">,
        /// Connection stays open but no samples are sent. Models a device
        /// that's reachable on the wire but has lost its data link to the
        /// metering / telemetry pipeline.
        Silent<"silent">,
        /// Stream task exits as soon as it sees this. Existing clients see
        /// EOF; new connections terminate immediately. Models an
        /// unreachable device.
        Closed<"closed">,
        /// Emit samples at the stream interval but with empty
        /// `metric_samples` and a single `ERROR` state snapshot. Models a
        /// device whose stream is alive but carries only an error state
        /// and no parseable metrics, so downstream consumers receive no
        /// data.
        ErrorEmpty<"error-empty">,
        /// The component exists in the graph (so clients discover and
        /// subscribe to it), but every telemetry-stream request is
        /// rejected with gRPC `NOT_FOUND`. Models a phantom component:
        /// present enough to be discovered and subscribed to, but with no
        /// data channel behind it, so a streaming client keeps retrying
        /// the subscription indefinitely.
        NotFound<"not-found">,
    }
}

tulisp::AsSymbol! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub enum CommandMode {
        /// Validate against bounds and apply.
        #[default]
        Normal<"normal">,
        /// Hang the request indefinitely. The client will time out
        /// according to its own deadline. Models a device whose control
        /// channel is alive but stuck.
        Timeout<"timeout">,
        /// Reply immediately with `Unavailable`. Models a device whose
        /// control channel is down.
        Error<"error">,
        /// Reject non-zero active-power setpoints with gRPC
        /// `INVALID_ARGUMENT`, even though the advertised bounds said the
        /// setpoint was within range. Models a device that advertises one
        /// set of bounds but then rejects a command of that size against a
        /// tighter internal limit. Faulting is intermittent and rotates
        /// per component (see `over_bound_faulty_now`). Zero-power
        /// (fail-safe) setpoints are still accepted.
        OverBound<"over-bound">,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ComponentRuntime {
    pub health: Health,
    pub telemetry: TelemetryMode,
    pub command: CommandMode,
}

impl Health {
    /// Proto state-code label corresponding to this health state.
    /// Returned to clients as part of every telemetry sample's
    /// state_snapshot when health is non-Ok; physics-derived labels
    /// take over when Ok.
    pub fn state_label(self) -> Option<&'static str> {
        match self {
            Self::Ok => None,
            Self::Error => Some("error"),
            Self::Standby => Some("standby"),
        }
    }
}

#[cfg(test)]
mod tests {
    use tulisp::{TulispContext, TulispConvertible};

    use super::{CommandMode, Health, TelemetryMode};

    #[test]
    fn a_symbol_reads_as_its_variant_and_a_string_does_not() {
        let mut ctx = TulispContext::new();
        let sym = ctx.intern("error");
        assert_eq!(Health::from_tulisp(&mut ctx, &sym).unwrap(), Health::Error);
        let sym = ctx.intern("silent");
        assert_eq!(
            TelemetryMode::from_tulisp(&mut ctx, &sym).unwrap(),
            TelemetryMode::Silent
        );
        let string = tulisp::TulispObject::from("error");
        let err = Health::from_tulisp(&mut ctx, &string).unwrap_err();
        assert!(err.desc().contains("Expected a symbol for Health"), "{err}");
    }

    #[test]
    fn an_unknown_symbol_names_the_accepted_ones() {
        let mut ctx = TulispContext::new();
        let sym = ctx.intern("borked");
        let err = CommandMode::from_tulisp(&mut ctx, &sym).unwrap_err();
        let msg = err.desc();
        assert!(msg.contains("unknown CommandMode 'borked'"), "{msg}");
        assert!(msg.contains("normal, timeout, error, over-bound"), "{msg}");
    }

    #[test]
    fn display_and_from_str_use_the_symbol_spelling() {
        assert_eq!(TelemetryMode::ErrorEmpty.to_string(), "error-empty");
        assert_eq!(
            "over-bound".parse::<CommandMode>().unwrap(),
            CommandMode::OverBound
        );
        assert!("OverBound".parse::<CommandMode>().is_err());
    }
}
