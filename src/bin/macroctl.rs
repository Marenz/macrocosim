//! Convenience CLI for poking at a running macrocosim server.
//!
//! gRPC commands (default --addr http://[::1]:8800):
//!   macroctl info
//!   macroctl list
//!   macroctl list --category battery
//!   macroctl tree
//!   macroctl stream 1001
//!   macroctl stream 1001 --samples 5 --json
//!   macroctl set-power 1001 8000
//!   macroctl set-power 1001 -5000 --lifetime 30   # negative → discharge
//!   macroctl augment-bounds 1001 --lower -15000 --upper 15000 --lifetime 60
//!   macroctl augment-bounds 1001 --lower -3000 --upper 3000 --reactive --lifetime 60
//!
//! Scenario commands — HTTP (default --ui-addr http://127.0.0.1:8801):
//!   macroctl scenario start "demo"
//!   macroctl scenario event outage "bat-1003"
//!   macroctl scenario load examples/scenario-driving.lisp
//!   macroctl scenario report
//!   macroctl scenario events --since 0 --limit 20
//!   macroctl scenario summary
//!   macroctl scenario stop

use std::collections::{BTreeMap, BTreeSet};

use clap::{Parser, Subcommand, ValueEnum};
use tonic::transport::Channel;

use macrocosim::proto::common::metrics::Metric;
use macrocosim::proto::common::microgrid::electrical_components::{
    ElectricalComponent, ElectricalComponentCategory,
    electrical_component_category_specific_info::Kind,
};
use macrocosim::proto::dispatch::microgrid_dispatch_service_client::MicrogridDispatchServiceClient;
use macrocosim::proto::dispatch::{
    CreateMicrogridDispatchRequest, DeleteMicrogridDispatchRequest, DispatchData,
    GetMicrogridDispatchRequest, ListMicrogridDispatchesRequest, UpdateMicrogridDispatchRequest,
    update_microgrid_dispatch_request::DispatchUpdate,
};
use macrocosim::proto::microgrid::microgrid_client::MicrogridClient;
use macrocosim::proto::microgrid::{
    AugmentElectricalComponentBoundsRequest, ListElectricalComponentConnectionsRequest,
    ListElectricalComponentsRequest, PowerType, ReceiveElectricalComponentTelemetryStreamRequest,
    SetElectricalComponentPowerRequest,
};
use macrocosim::sim::dispatch::{json_to_struct, parse_target, struct_to_json, target_to_string};

#[derive(Parser, Debug)]
#[command(
    name = "macroctl",
    about = "Macrocosim microgrid client",
    version,
    propagate_version = true
)]
struct Cli {
    /// gRPC endpoint of the simulator.
    #[arg(long, default_value = "http://[::1]:8800", global = true)]
    addr: String,

    /// HTTP endpoint of the simulator's UI server. Used by the
    /// `scenario` subcommand — the scenario lifecycle isn't on
    /// gRPC.
    #[arg(long, default_value = "http://127.0.0.1:8801", global = true)]
    ui_addr: String,

    /// gRPC endpoint of the (enterprise-wide) MicrogridDispatchService.
    /// Used by the `dispatch` subcommand; separate from `--addr` since
    /// the dispatch service binds its own shared port.
    #[arg(long, default_value = "http://[::1]:8900", global = true)]
    dispatch_addr: String,

    /// Emit JSON instead of human-friendly output where applicable.
    #[arg(long, global = true)]
    json: bool,

    /// Target microgrid id for per-microgrid HTTP-routed subcommands
    /// (`pool`, `dashboard --tail`, `snapshot`). When set, macroctl
    /// reads from `/api/mg/{id}/...` instead of the legacy
    /// single-microgrid `/api/...` paths. Default = the lowest id
    /// reported by `GET /api/microgrids`, which matches the
    /// behaviour of the legacy paths in a single-microgrid binary.
    #[arg(long, global = true)]
    microgrid_id: Option<u64>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show microgrid metadata.
    Info,

    /// List electrical components.
    List {
        /// Filter by category.
        #[arg(long)]
        category: Option<Category>,
        /// Filter by component ID (repeatable).
        #[arg(long = "id")]
        ids: Vec<u64>,
    },

    /// List electrical-component connections.
    Connections {
        /// Filter by source ID (repeatable).
        #[arg(long = "from")]
        from: Vec<u64>,
        /// Filter by destination ID (repeatable).
        #[arg(long = "to")]
        to: Vec<u64>,
    },

    /// Pretty-print the topology as a tree.
    Tree,

    /// Stream telemetry samples for a component.
    Stream {
        /// Component ID.
        id: u64,
        /// Stop after N samples (default: stream forever, Ctrl+C to exit).
        #[arg(long)]
        samples: Option<usize>,
        /// Subscribe only to specific metrics. Repeatable. Names are
        /// case-insensitive and match the proto enum either with or
        /// without the `METRIC_` prefix (e.g. `dc_power` or
        /// `METRIC_DC_POWER`). Omitting the flag streams every metric.
        #[arg(long = "metric")]
        metrics: Vec<String>,
    },

    /// Set the active or reactive power set-point on a component.
    SetPower {
        /// Component ID.
        id: u64,
        /// Power in watts (or VARs with --reactive). Negative = discharge.
        #[arg(allow_hyphen_values = true)]
        power: f32,
        /// Treat power as reactive (VAR) rather than active (W).
        #[arg(long)]
        reactive: bool,
        /// Request lifetime in seconds (10..=900).
        #[arg(long)]
        lifetime: Option<u64>,
    },

    /// Augment a component's active- or reactive-power bounds for a
    /// limited time.
    AugmentBounds {
        /// Component ID.
        id: u64,
        /// New lower bound (W, or VAR with --reactive).
        #[arg(long, allow_hyphen_values = true)]
        lower: f32,
        /// New upper bound (W, or VAR with --reactive).
        #[arg(long, allow_hyphen_values = true)]
        upper: f32,
        /// Target the reactive (Q) axis instead of active (P). The
        /// server validates the bounds against the live Q envelope at
        /// the component's current P.
        #[arg(long)]
        reactive: bool,
        /// Request lifetime in seconds (10..=900).
        #[arg(long, default_value_t = 60)]
        lifetime: u64,
    },

    /// Create / list / pause / resume / delete dispatches via the
    /// MicrogridDispatchService gRPC API (default --dispatch-addr
    /// http://[::1]:8900).
    #[command(subcommand)]
    Dispatch(DispatchCmd),

    /// Drive the running scenario: start / stop / event / load,
    /// and read back report + events. Talks to the UI's HTTP
    /// surface, not gRPC.
    #[command(subcommand)]
    Scenario(ScenarioCmd),

    /// Save / load / list snapshots of one microgrid's file.
    /// Picks the microgrid with the global --microgrid-id, or the
    /// lowest registered one. Routes through the UI HTTP surface;
    /// gRPC isn't touched.
    #[command(subcommand)]
    Snapshot(SnapshotCmd),

    /// Read the loopback BatteryPool / PvPool aggregates. Output
    /// matches the `Sample<Q>` shape frequenz-microgrid's pool
    /// streams yield, so the output is paste-into-a-bug-report
    /// compatible with what a downstream control app sees.
    Pool {
        #[command(subcommand)]
        cmd: PoolCmd,
    },

    /// Terminal-resident pulse bar — one line per second showing
    /// component health, the grid / PV / battery readouts, and the
    /// loopback Microgrid status. Useful in a tmux pane next to
    /// the editor. Polls the UI HTTP surface; gRPC isn't touched.
    Dashboard {
        /// Stream forever instead of printing a single snapshot
        /// and exiting.
        #[arg(long)]
        tail: bool,
        /// Polling cadence in seconds (only with --tail).
        #[arg(long, default_value_t = 1.0)]
        interval: f64,
    },
}

#[derive(Subcommand, Debug)]
enum DispatchCmd {
    /// List a microgrid's dispatches (newest created first).
    List {
        /// Microgrid id.
        microgrid_id: u64,
    },
    /// Show a single dispatch.
    Get {
        /// Microgrid id.
        microgrid_id: u64,
        /// Dispatch id.
        dispatch_id: u64,
    },
    /// Create a dispatch. TARGET is a comma-separated list of either
    /// component categories (battery, grid, meter, inverter,
    /// ev_charger, chp, steam_boiler) or numeric component ids
    /// (e.g. "1,2,3").
    Create {
        /// Microgrid id.
        microgrid_id: u64,
        /// Dispatch type the downstream actor keys on.
        #[arg(name = "type")]
        type_: String,
        /// Target components.
        target: String,
        /// Duration in seconds; omit for an indefinite dispatch.
        #[arg(long)]
        duration: Option<u32>,
        /// JSON payload object, e.g. '{"target_power_w": 5000}'.
        #[arg(long)]
        payload: Option<String>,
        /// Create the dispatch inactive (default: active).
        #[arg(long)]
        inactive: bool,
        /// Mark the dispatch as a dry run.
        #[arg(long)]
        dry_run: bool,
        /// RFC3339 start time; omit to start immediately (now).
        #[arg(long)]
        start: Option<String>,
    },
    /// Pause a dispatch (set it inactive).
    Pause {
        /// Microgrid id.
        microgrid_id: u64,
        /// Dispatch id.
        dispatch_id: u64,
    },
    /// Resume a paused dispatch (set it active).
    Resume {
        /// Microgrid id.
        microgrid_id: u64,
        /// Dispatch id.
        dispatch_id: u64,
    },
    /// Delete a dispatch.
    Delete {
        /// Microgrid id.
        microgrid_id: u64,
        /// Dispatch id.
        dispatch_id: u64,
    },
}

#[derive(Subcommand, Debug)]
enum PoolCmd {
    /// Battery pool — power + active-power envelope.
    Battery {
        /// Stream forever; otherwise prints one snapshot and exits.
        #[arg(long)]
        stream: bool,
    },
    /// PV pool — current aggregate active power. (frequenz-microgrid
    /// 0.4.1 has no PvPool envelope, so this is just `pv_power`.)
    Pv {
        #[arg(long)]
        stream: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SnapshotCmd {
    /// Copy the microgrid's file to snapshots/MG_ID/NAME.lisp.
    Save {
        /// Snapshot name (file-name component only; no slashes).
        name: String,
    },
    /// Restore snapshots/MG_ID/NAME.lisp over the microgrid's file
    /// and reload it.
    Load {
        name: String,
        /// Load the snapshot as a NEW microgrid under this id
        /// instead of restoring it over the original.
        #[arg(long)]
        as_id: Option<u64>,
    },
    /// List the microgrid's snapshots, alphabetical.
    List,
}

#[derive(Subcommand, Debug)]
enum ScenarioCmd {
    /// List every registered scenario with its schedule / clock /
    /// length and per-section counts (`GET /api/scenarios`).
    List,

    /// Run a registered scenario. Default (live) starts it on the
    /// running server's wall clock. `--stepped` instead boots a
    /// headless deterministic run in-process from `--config` and
    /// prints the report — the CI gate; pair with `--assert`.
    Run {
        /// Registered scenario name.
        name: String,
        /// Run headless + deterministic on a sim clock, in-process,
        /// rather than against the live server.
        #[arg(long)]
        stepped: bool,
        /// Config file to boot for a `--stepped` run (the one that
        /// registered the scenario).
        #[arg(long)]
        config: Option<String>,
        /// `--stepped` clock step in milliseconds (default 100).
        #[arg(long)]
        step: Option<u64>,
        /// Seconds: the `--stepped` run length, or the `--wait` cap for
        /// a live run. Defaults to the scenario's `:length`.
        #[arg(long)]
        until: Option<u64>,
        /// Live runs only: block until the scenario finishes (its
        /// `:length`, or `--until`), then stop it — so `--assert` can
        /// gate on the result.
        #[arg(long)]
        wait: bool,
        /// Exit non-zero if any `(scenario-expect …)` check failed.
        /// Needs `--stepped` or `--wait` (a bare live run is async).
        #[arg(long)]
        assert: bool,
    },

    /// Begin a fresh scenario. Resets the journal + reporters.
    Start {
        /// Scenario name. Lands as `(scenario-start NAME)`.
        name: String,
    },

    /// Mark the running scenario as ended. Freezes elapsed +
    /// metrics; flushes any active CSV sinks.
    Stop,

    /// Append a journal event. KIND becomes a Lisp symbol; PAYLOAD
    /// is rendered as a string.
    Event {
        /// Event kind (e.g. `outage`, `note`).
        kind: String,
        /// Free-form payload string.
        payload: String,
    },

    /// Load a hand-written scenario file via `(load PATH)`. Path
    /// is resolved against the server's state dir, same as
    /// `(load …)` from the REPL.
    Load {
        /// Path to the scenario file (e.g. `examples/scenario-driving.lisp`).
        path: String,
    },

    /// Show lifecycle summary — name, started_at, elapsed, event
    /// count.
    Summary,

    /// Show aggregate metrics: peak / charge / discharge / SoC
    /// stats / 15-min averages.
    Report {
        /// Exit non-zero if any `(scenario-expect …)` check failed —
        /// turns a scenario run into a CI gate.
        #[arg(long)]
        assert: bool,
    },

    /// Show recent events in the journal.
    Events {
        /// Cursor: only events with id >= SINCE. Default 0.
        #[arg(long, default_value_t = 0)]
        since: u64,
        /// Cap on returned entries.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum Category {
    Grid,
    Meter,
    Inverter,
    Battery,
    EvCharger,
    Chp,
    SteamBoiler,
}

impl Category {
    fn to_proto(self) -> ElectricalComponentCategory {
        match self {
            Self::Grid => ElectricalComponentCategory::GridConnectionPoint,
            Self::Meter => ElectricalComponentCategory::Meter,
            Self::Inverter => ElectricalComponentCategory::Inverter,
            Self::Battery => ElectricalComponentCategory::Battery,
            Self::EvCharger => ElectricalComponentCategory::EvCharger,
            Self::Chp => ElectricalComponentCategory::Chp,
            Self::SteamBoiler => ElectricalComponentCategory::SteamBoiler,
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.cmd {
        // HTTP-backed commands dispatch directly — they only need
        // the HTTP client, not a live gRPC channel. Avoids paying
        // for a failing gRPC connect when the user only wants
        // /api/scenario.
        Cmd::Dispatch(d) => run_dispatch(d, &cli.dispatch_addr, cli.json).await,
        Cmd::Scenario(s) => run_scenario(s, &cli.ui_addr, cli.json).await,
        Cmd::Snapshot(s) => run_snapshot(s, &cli.ui_addr, cli.microgrid_id, cli.json).await,
        Cmd::Dashboard { tail, interval } => {
            run_dashboard(&cli.ui_addr, cli.microgrid_id, tail, interval).await
        }
        Cmd::Pool { cmd } => run_pool(cmd, &cli.ui_addr, cli.microgrid_id, cli.json).await,
        // Everything else talks the Microgrid gRPC API: connect
        // once, then dispatch.
        cmd => {
            let mut client = MicrogridClient::connect(cli.addr.clone()).await?;
            match cmd {
                Cmd::Info => cmd_info(&mut client, cli.json).await,
                Cmd::List { category, ids } => cmd_list(&mut client, category, ids, cli.json).await,
                Cmd::Connections { from, to } => {
                    cmd_connections(&mut client, from, to, cli.json).await
                }
                Cmd::Tree => cmd_tree(&mut client).await,
                Cmd::Stream {
                    id,
                    samples,
                    metrics,
                } => cmd_stream(&mut client, id, samples, metrics, cli.json).await,
                Cmd::SetPower {
                    id,
                    power,
                    reactive,
                    lifetime,
                } => cmd_set_power(&mut client, id, power, reactive, lifetime).await,
                Cmd::AugmentBounds {
                    id,
                    lower,
                    upper,
                    reactive,
                    lifetime,
                } => cmd_augment(&mut client, id, lower, upper, reactive, lifetime).await,
                _ => unreachable!("HTTP-backed commands are dispatched above"),
            }
        }
    }
}

fn str_err(e: String) -> Box<dyn std::error::Error> {
    e.into()
}

fn rfc3339_to_ts(s: &str) -> Result<prost_types::Timestamp, Box<dyn std::error::Error>> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)?.with_timezone(&chrono::Utc);
    Ok(macrocosim::proto_conv::datetime_to_ts(dt))
}

/// One-line dispatch summary for `dispatch list`.
fn dispatch_summary(d: &macrocosim::proto::dispatch::Dispatch) -> String {
    let id = d.metadata.as_ref().map(|m| m.dispatch_id).unwrap_or(0);
    let data = d.data.as_ref();
    let ty = data.map(|x| x.r#type.as_str()).unwrap_or("?");
    let active = data.map(|x| x.is_active).unwrap_or(false);
    let dry = if data.map(|x| x.is_dry_run).unwrap_or(false) {
        " dry-run"
    } else {
        ""
    };
    let status = if active { "active" } else { "inactive" };
    let dur = data
        .and_then(|x| x.duration)
        .map(|s| format!("{s}s"))
        .unwrap_or_else(|| "indefinite".to_string());
    let target = target_to_string(data.and_then(|x| x.target.as_ref()));
    format!("#{id}  {ty}  [{status}{dry}]  dur={dur}  target={target}")
}

/// Multi-line dispatch detail for `dispatch get` / `create`.
fn print_dispatch_detail(d: &macrocosim::proto::dispatch::Dispatch) {
    println!("{}", dispatch_summary(d));
    if let Some(data) = d.data.as_ref() {
        if let Some(st) = data.start_time.as_ref() {
            println!("  start   = {}", format_ts(st));
        }
        if let Some(p) = data.payload.as_ref() {
            println!("  payload = {}", struct_to_json(p));
        }
    }
    if let Some(end) = d.metadata.as_ref().and_then(|m| m.end_time.as_ref()) {
        println!("  end     = {}", format_ts(end));
    }
}

/// Pause (`active=false`) / resume (`active=true`) a dispatch via an
/// `is_active` field-mask update.
async fn set_active_dispatch(
    client: &mut MicrogridDispatchServiceClient<Channel>,
    microgrid_id: u64,
    dispatch_id: u64,
    active: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = client
        .update_microgrid_dispatch(UpdateMicrogridDispatchRequest {
            microgrid_id,
            dispatch_id,
            update_mask: Some(prost_types::FieldMask {
                paths: vec!["is_active".to_string()],
            }),
            update: Some(DispatchUpdate {
                is_active: Some(active),
                ..Default::default()
            }),
        })
        .await?
        .into_inner();
    if json {
        println!("{resp:#?}");
    } else {
        let verb = if active { "resumed" } else { "paused" };
        println!("{verb} dispatch {dispatch_id} (microgrid {microgrid_id})");
    }
    Ok(())
}

async fn run_dispatch(
    cmd: DispatchCmd,
    addr: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = MicrogridDispatchServiceClient::connect(addr.to_string()).await?;
    match cmd {
        DispatchCmd::List { microgrid_id } => {
            let resp = client
                .list_microgrid_dispatches(ListMicrogridDispatchesRequest {
                    microgrid_id,
                    filter: None,
                    sort_options: None,
                    pagination_params: None,
                })
                .await?
                .into_inner();
            if json {
                println!("{resp:#?}");
                return Ok(());
            }
            if resp.dispatches.is_empty() {
                println!("(no dispatches for microgrid {microgrid_id})");
            }
            for d in &resp.dispatches {
                println!("{}", dispatch_summary(d));
            }
            Ok(())
        }
        DispatchCmd::Get {
            microgrid_id,
            dispatch_id,
        } => {
            let resp = client
                .get_microgrid_dispatch(GetMicrogridDispatchRequest {
                    microgrid_id,
                    dispatch_id,
                })
                .await?
                .into_inner();
            if json {
                println!("{resp:#?}");
                return Ok(());
            }
            match resp.dispatch.as_ref() {
                Some(d) => print_dispatch_detail(d),
                None => println!("(dispatch {dispatch_id} not found)"),
            }
            Ok(())
        }
        DispatchCmd::Create {
            microgrid_id,
            type_,
            target,
            duration,
            payload,
            inactive,
            dry_run,
            start,
        } => {
            let target = parse_target(&target).map_err(str_err)?;
            let payload = match payload {
                Some(s) => {
                    let value: serde_json::Value = serde_json::from_str(&s)?;
                    Some(json_to_struct(&value).map_err(str_err)?)
                }
                None => None,
            };
            let start_immediately = start.is_none();
            let start_time = match start.as_deref() {
                Some(s) => Some(rfc3339_to_ts(s)?),
                None => None,
            };
            let data = DispatchData {
                r#type: type_,
                start_time,
                duration,
                target: Some(target),
                is_active: !inactive,
                is_dry_run: dry_run,
                payload,
                recurrence: None,
            };
            let resp = client
                .create_microgrid_dispatch(CreateMicrogridDispatchRequest {
                    microgrid_id,
                    dispatch_data: Some(data),
                    // None (not Some(false)) when a start time was
                    // given — absent is the proto's "not requested".
                    start_immediately: start_immediately.then_some(true),
                })
                .await?
                .into_inner();
            if json {
                println!("{resp:#?}");
                return Ok(());
            }
            match resp.dispatch.as_ref() {
                Some(d) => {
                    println!("created:");
                    print_dispatch_detail(d);
                }
                None => println!("(no dispatch returned)"),
            }
            Ok(())
        }
        DispatchCmd::Pause {
            microgrid_id,
            dispatch_id,
        } => set_active_dispatch(&mut client, microgrid_id, dispatch_id, false, json).await,
        DispatchCmd::Resume {
            microgrid_id,
            dispatch_id,
        } => set_active_dispatch(&mut client, microgrid_id, dispatch_id, true, json).await,
        DispatchCmd::Delete {
            microgrid_id,
            dispatch_id,
        } => {
            let resp = client
                .delete_microgrid_dispatch(DeleteMicrogridDispatchRequest {
                    microgrid_id,
                    dispatch_id,
                })
                .await?
                .into_inner();
            if json {
                println!("{resp:#?}");
            } else {
                println!(
                    "deleted dispatch {} (microgrid {})",
                    resp.dispatch_id, resp.microgrid_id
                );
            }
            Ok(())
        }
    }
}

fn print_scenarios(resp: &serde_json::Value) {
    let Some(arr) = resp.as_array() else {
        println!("(unexpected response shape)");
        return;
    };
    if arr.is_empty() {
        println!("(no scenarios registered)");
        return;
    }
    for s in arr {
        let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let desc = s.get("description").and_then(|v| v.as_str()).unwrap_or("");
        let schedule = s.get("schedule").and_then(|v| v.as_str()).unwrap_or("?");
        let clock = s.get("clock").and_then(|v| v.as_str()).unwrap_or("?");
        let count = |k: &str| s.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        let len = match s.get("length_s").and_then(|v| v.as_f64()) {
            Some(secs) => format!("{secs:.0}s"),
            None => "open".to_owned(),
        };
        // schedule/clock + run length, then the populated sections.
        let sections = [
            (
                "setup",
                u64::from(s.get("has_setup").and_then(|v| v.as_bool()) == Some(true)),
            ),
            ("drive", count("n_drive")),
            ("agents", count("n_agents")),
            ("cues", count("n_cues")),
            ("expect", count("n_expect")),
            (
                "record",
                u64::from(s.get("records").and_then(|v| v.as_bool()) == Some(true)),
            ),
        ]
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| format!("{k}={n}"))
        .collect::<Vec<_>>()
        .join(" ");
        println!("{name}  [{schedule}/{clock} {len}]  {sections}  {desc}");
    }
}

async fn run_pool(
    cmd: PoolCmd,
    ui_addr: &str,
    microgrid_id: Option<u64>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::new();
    let mg = resolve_microgrid_id(&http, ui_addr, microgrid_id).await?;
    let (kind, stream) = match cmd {
        PoolCmd::Battery { stream } => ("battery", stream),
        PoolCmd::Pv { stream } => ("pv", stream),
    };
    loop {
        // Same transient-error tolerance as dashboard --tail.
        match build_pool_line(&http, ui_addr, mg, kind, json).await {
            Ok(line) => println!("{line}"),
            Err(e) if stream => eprintln!("macroctl: pool fetch failed ({e}); retrying"),
            Err(e) => return Err(e),
        }
        if !stream {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Resolve the microgrid id macroctl should hit for per-mg HTTP
/// endpoints. Honours an explicit `--microgrid-id N` if supplied,
/// otherwise queries `/api/microgrids` and returns the lowest
/// registered id — matches the legacy "first registry entry"
/// fallback the server-side router uses.
async fn resolve_microgrid_id(
    http: &reqwest::Client,
    ui_addr: &str,
    explicit: Option<u64>,
) -> Result<u64, Box<dyn std::error::Error>> {
    if let Some(id) = explicit {
        return Ok(id);
    }
    let list: serde_json::Value =
        checked(http.get(format!("{ui_addr}/api/microgrids")).send().await?)
            .await?
            .json()
            .await?;
    list.as_array()
        .and_then(|a| a.iter().filter_map(|m| m.get("id")?.as_u64()).min())
        .ok_or_else(|| "no microgrids registered".into())
}

async fn build_pool_line(
    http: &reqwest::Client,
    ui_addr: &str,
    mg_id: u64,
    kind: &str,
    json: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let latest: serde_json::Value = checked(
        http.get(format!("{ui_addr}/api/mg/{mg_id}/microgrid/latest"))
            .send()
            .await?,
    )
    .await?
    .json()
    .await?;
    let stream_for = |s: &str| latest.get(s).cloned();
    if json {
        let mut out = serde_json::Map::new();
        match kind {
            "battery" => {
                if let Some(v) = stream_for("battery_pool_power") {
                    out.insert("power".into(), v);
                }
                if let Some(v) = stream_for("battery_pool_bounds_lower") {
                    out.insert("bounds_lower".into(), v);
                }
                if let Some(v) = stream_for("battery_pool_bounds_upper") {
                    out.insert("bounds_upper".into(), v);
                }
            }
            "pv" => {
                if let Some(v) = stream_for("pv_power") {
                    out.insert("power".into(), v);
                }
            }
            _ => {}
        }
        return Ok(serde_json::to_string(&serde_json::Value::Object(out))?);
    }
    let now = chrono::Local::now().format("%H:%M:%S");
    let read = |s: &str| latest_value(&latest, s);
    let fmt = |w: Option<f64>| match w {
        None => "—".to_owned(),
        Some(v) if v.abs() >= 1e6 => format!("{:+.2} MW", v / 1e6),
        Some(v) if v.abs() >= 1e3 => format!("{:+.2} kW", v / 1e3),
        Some(v) => format!("{:+.1} W", v),
    };
    match kind {
        "battery" => Ok(format!(
            "{now}  power={}  envelope=[{} → {}]",
            fmt(read("battery_pool_power")),
            fmt(read("battery_pool_bounds_lower")),
            fmt(read("battery_pool_bounds_upper")),
        )),
        "pv" => Ok(format!("{now}  power={}", fmt(read("pv_power")))),
        _ => Ok(String::new()),
    }
}

/// Polls /api/topology + /api/microgrid/latest at `interval`
/// seconds and prints a one-line pulse summary per tick. With
/// `tail=false` (single snapshot mode) the loop runs once and
/// exits, matching `macroctl dashboard` without a flag.
async fn run_dashboard(
    ui_addr: &str,
    microgrid_id: Option<u64>,
    tail: bool,
    interval: f64,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::new();
    let mg = resolve_microgrid_id(&http, ui_addr, microgrid_id).await?;
    let dt = std::time::Duration::from_secs_f64(interval.max(0.1));
    loop {
        // In --tail mode a transient HTTP error (server mid-reload,
        // loopback rebuilding) must not abort an overnight watch —
        // warn and try again next tick. One-shot mode still errors.
        match build_dashboard_line(&http, ui_addr, mg).await {
            Ok(line) => println!("{line}"),
            Err(e) if tail => eprintln!("macroctl: dashboard fetch failed ({e}); retrying"),
            Err(e) => return Err(e),
        }
        if !tail {
            return Ok(());
        }
        tokio::time::sleep(dt).await;
    }
}

async fn build_dashboard_line(
    http: &reqwest::Client,
    ui_addr: &str,
    mg_id: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let topo: serde_json::Value = checked(
        http.get(format!("{ui_addr}/api/mg/{mg_id}/topology"))
            .send()
            .await?,
    )
    .await?
    .json()
    .await?;
    let latest: serde_json::Value = checked(
        http.get(format!("{ui_addr}/api/mg/{mg_id}/microgrid/latest"))
            .send()
            .await?,
    )
    .await?
    .json()
    .await?;
    let now = chrono::Local::now().format("%H:%M:%S");
    let components = topo
        .get("components")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut ok = 0u32;
    let mut err = 0u32;
    let mut bat_socs: Vec<f64> = Vec::new();
    for c in &components {
        match c.get("health").and_then(|h| h.as_str()) {
            Some("ok") => ok += 1,
            Some(_) => err += 1,
            None => {}
        }
        if c.get("category").and_then(|v| v.as_str()) == Some("battery") {
            // SoC isn't on /api/topology; the histogram column lands
            // when J5's pool stream gets folded in. Keep the field
            // for forward compatibility.
            if let Some(soc) = c.get("soc").and_then(|v| v.as_f64()) {
                bat_socs.push(soc);
            }
        }
    }
    let total = components.len();
    let pick = |stream| latest_value(&latest, stream);
    let grid = pick("grid_power");
    let pv = pick("pv_power");
    let bat = pick("battery_pool_power");
    let loopback = latest.as_object().map(|m| !m.is_empty()).unwrap_or(false);
    let fmt = |watts: Option<f64>| match watts {
        None => "—".to_owned(),
        Some(v) if v.abs() >= 1e6 => format!("{:.2}MW", v / 1e6),
        Some(v) if v.abs() >= 1e3 => format!("{:.1}kW", v / 1e3),
        Some(v) => format!("{:.0}W", v),
    };
    let bat_summary = if bat_socs.is_empty() {
        String::new()
    } else {
        let min = bat_socs.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = bat_socs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        format!(" soc=[{:.0}-{:.0}%]", min, max)
    };
    Ok(format!(
        "{now}  comp={total} ok={ok} err={err}  grid={} pv={} bat={}{bat_summary}  loopback={}",
        fmt(grid),
        fmt(pv),
        fmt(bat),
        if loopback { "connected" } else { "off" },
    ))
}

/// Fetch `/api/scenario/report`, print it, and — with `assert` — return
/// an error when any check failed. Shared by the live `--wait` gate and
/// the standalone `scenario report` command so the two stay in lockstep.
async fn fetch_print_assert_report(
    http: &reqwest::Client,
    ui_addr: &str,
    json: bool,
    assert: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let r: serde_json::Value = checked(
        http.get(format!("{ui_addr}/api/scenario/report"))
            .send()
            .await?,
    )
    .await?
    .json()
    .await?;
    print_report(&r, json);
    if assert {
        let failed = r["checks_failed"].as_u64().unwrap_or(0);
        if failed > 0 {
            return Err(format!("{failed} scenario check(s) failed").into());
        }
    }
    Ok(())
}

async fn run_scenario(
    cmd: ScenarioCmd,
    ui_addr: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::new();
    match cmd {
        ScenarioCmd::List => {
            let resp: serde_json::Value =
                checked(http.get(format!("{ui_addr}/api/scenarios")).send().await?)
                    .await?
                    .json()
                    .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_scenarios(&resp);
            }
        }
        ScenarioCmd::Run {
            name,
            stepped,
            config,
            step,
            until,
            wait,
            assert,
        } => {
            if stepped {
                // Headless, deterministic, in-process — no server. Boots
                // the config that registered the scenario, runs on a sim
                // clock, prints the report, and (with --assert) exits
                // non-zero on any failed check: the CI gate.
                let config = config.ok_or("`scenario run --stepped` needs --config <file>")?;
                let dt = std::time::Duration::from_millis(step.unwrap_or(100));
                let (cfg, _clock) = macrocosim::lisp::Config::new_headless(&config)
                    .map_err(|e| format!("headless boot failed: {e}"))?;
                // `--until` overrides the scenario's own `:length`;
                // without it the declared length is the run length.
                // Same runner either way, so both paths tear down —
                // cancel timers + restore driven knobs — identically.
                let steps =
                    cfg.run_scenario_stepped(&name, dt, until.map(std::time::Duration::from_secs))?;
                let rv = cfg.scenario_report_json();
                if json {
                    println!("{}", serde_json::to_string_pretty(&rv)?);
                } else {
                    print_report(&rv, false);
                    eprintln!("(stepped {steps} steps)");
                }
                let failed = rv["checks_failed"].as_u64().unwrap_or(0);
                if assert && failed > 0 {
                    return Err(format!("{failed} scenario check(s) failed").into());
                }
            } else {
                // Live: start on the running server's wall clock.
                if assert && !wait {
                    return Err(
                        "`--assert` on a live run needs `--wait` (or use `--stepped`)".into(),
                    );
                }
                // Resolve the wait length BEFORE starting, so a --wait on
                // a scenario with no :length and no --until fails fast
                // instead of leaving the run orphaned server-side with
                // nothing to stop it.
                let wait_secs = if wait {
                    Some(match until {
                        Some(s) => s,
                        None => {
                            // Default to the scenario's declared :length.
                            let list: serde_json::Value =
                                checked(http.get(format!("{ui_addr}/api/scenarios")).send().await?)
                                    .await?
                                    .json()
                                    .await?;
                            let len = list
                                .as_array()
                                .and_then(|a| a.iter().find(|s| s["name"] == name.as_str()))
                                .and_then(|s| s["length_s"].as_f64());
                            match len {
                                Some(l) => l.ceil() as u64,
                                None => {
                                    return Err(
                                        "live `--wait` needs `--until` for a scenario with no :length"
                                            .into(),
                                    );
                                }
                            }
                        }
                    })
                } else {
                    None
                };
                checked(
                    http.post(format!("{ui_addr}/api/scenarios/{name}/start"))
                        .send()
                        .await?,
                )
                .await?;
                if !wait {
                    // Fire-and-forget; the run continues server-side.
                    println!("scenario run {name} (live)");
                    return Ok(());
                }
                // --wait: block until the scenario finishes (the journal
                // reports ended, or elapsed reaches the run length), then
                // stop it so the report freezes + any CSV sinks flush.
                let wait_secs = wait_secs.expect("wait ⇒ wait_secs resolved above");
                eprintln!("waiting up to {wait_secs}s for {name} to finish…");
                let deadline = wait_secs + 5; // small grace past the run length
                let mut waited = 0u64;
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    waited += 1;
                    // Tolerate a transient poll hiccup (a momentary
                    // non-2xx while the loopback rebuilds): treat it as
                    // "not done yet" rather than aborting the run — the
                    // deadline below still bounds the wait.
                    let done = match http
                        .get(format!("{ui_addr}/api/scenario"))
                        .send()
                        .await
                        .and_then(|r| r.error_for_status())
                    {
                        Ok(resp) => match resp.json::<serde_json::Value>().await {
                            Ok(s) => {
                                let ended = !s["ended_at"].is_null();
                                let elapsed = s["elapsed_s"].as_f64().unwrap_or(0.0);
                                ended || elapsed >= wait_secs as f64
                            }
                            Err(_) => false,
                        },
                        Err(_) => false,
                    };
                    if done || waited >= deadline {
                        break;
                    }
                }
                // Freeze the report + flush any CSV sinks. A failed stop
                // means the report may still be mutating, so surface it
                // rather than asserting on a half-frozen run.
                checked(
                    http.post(format!("{ui_addr}/api/scenarios/stop"))
                        .send()
                        .await?,
                )
                .await
                .map_err(|e| format!("failed to stop scenario: {e}"))?;
                fetch_print_assert_report(&http, ui_addr, json, assert).await?;
            }
        }
        ScenarioCmd::Start { name } => {
            // Quote the name as a Lisp string. We don't accept
            // arbitrary expressions here on purpose — names are
            // labels, not code.
            eval(
                &http,
                ui_addr,
                &format!("(scenario-start {})", lisp_string(&name)),
            )
            .await?;
            println!("scenario-start {name}");
        }
        ScenarioCmd::Stop => {
            eval(&http, ui_addr, "(scenario-stop)").await?;
            println!("scenario-stopped");
        }
        ScenarioCmd::Event { kind, payload } => {
            // KIND → unquoted symbol (matches the (scenario-event
            // 'symbol …) idiom in scenario scripts). PAYLOAD →
            // Lisp string. Kinds are labels, not code — reject
            // anything that would not read back as one symbol.
            // Requiring a leading letter keeps every accepted kind
            // out of the reader's number shapes ("42", "-5",
            // "5_000", "1e3" all read back as numbers, not symbols).
            if !kind.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                || !kind
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!(
                    "event kind must be a plain symbol (a letter, then letters, \
                     digits, - or _): {kind:?}"
                )
                .into());
            }
            let id = eval(
                &http,
                ui_addr,
                &format!("(scenario-event '{kind} {})", lisp_string(&payload)),
            )
            .await?;
            println!("event id={id}");
        }
        ScenarioCmd::Load { path } => {
            eval(&http, ui_addr, &format!("(load {})", lisp_string(&path))).await?;
            println!("loaded {path}");
        }
        ScenarioCmd::Summary => {
            let s: serde_json::Value =
                checked(http.get(format!("{ui_addr}/api/scenario")).send().await?)
                    .await?
                    .json()
                    .await?;
            print_summary(&s, json);
        }
        ScenarioCmd::Report { assert } => {
            fetch_print_assert_report(&http, ui_addr, json, assert).await?;
        }
        ScenarioCmd::Events { since, limit } => {
            let e: serde_json::Value = checked(
                http.get(format!("{ui_addr}/api/scenario/events"))
                    .query(&[("since", since.to_string()), ("limit", limit.to_string())])
                    .send()
                    .await?,
            )
            .await?
            .json()
            .await?;
            print_events(&e, json);
        }
    }
    Ok(())
}

async fn run_snapshot(
    cmd: SnapshotCmd,
    ui_addr: &str,
    microgrid_id: Option<u64>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::new();
    // Snapshots belong to one microgrid: they copy that microgrid's
    // own file. Same selection rule as the other per-mg subcommands —
    // --microgrid-id, else the lowest registered id.
    let mg = resolve_microgrid_id(&http, ui_addr, microgrid_id).await?;
    match cmd {
        SnapshotCmd::Save { name } => {
            let resp: serde_json::Value = checked(
                http.post(format!("{ui_addr}/api/mg/{mg}/snapshots/save"))
                    .json(&serde_json::json!({ "name": name }))
                    .send()
                    .await?,
            )
            .await?
            .json()
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                println!(
                    "saved {} -> {}",
                    name,
                    resp.get("path").and_then(|v| v.as_str()).unwrap_or("?"),
                );
            }
        }
        SnapshotCmd::Load { name, as_id } => {
            let mut body = serde_json::json!({ "name": name });
            if let Some(id) = as_id {
                body["as_id"] = serde_json::json!(id);
            }
            let resp: serde_json::Value = checked(
                http.post(format!("{ui_addr}/api/mg/{mg}/snapshots/load"))
                    .json(&body)
                    .send()
                    .await?,
            )
            .await?
            .json()
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                let loaded = resp.get("id").and_then(|v| v.as_u64()).unwrap_or(mg);
                println!("loaded {name} into microgrid {loaded}");
            }
        }
        SnapshotCmd::List => {
            let resp: serde_json::Value = checked(
                http.get(format!("{ui_addr}/api/mg/{mg}/snapshots"))
                    .send()
                    .await?,
            )
            .await?
            .json()
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                let names = resp
                    .get("snapshots")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if names.is_empty() {
                    println!("(no snapshots for microgrid {mg})");
                } else {
                    for n in names {
                        if let Some(s) = n.as_str() {
                            println!("{s}");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// POST a Lisp expression to the whole-site `/api/eval`. Returns
/// the printed value, or the evaluation error.
async fn eval(
    http: &reqwest::Client,
    ui_addr: &str,
    expr: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let resp = http
        .post(format!("{ui_addr}/api/eval"))
        .body(expr.to_string())
        .send()
        .await?;
    let body: serde_json::Value = checked(resp)
        .await
        .map_err(|e| format!("eval failed: {e}"))?
        .json()
        .await?;
    Ok(body["value"].as_str().unwrap_or("").to_string())
}

/// `resp` when it succeeded; otherwise an error carrying the
/// server's `error` text, or the body or status when there is none.
async fn checked(resp: reqwest::Response) -> Result<reqwest::Response, Box<dyn std::error::Error>> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().await.unwrap_or_default();
    Err(error_line(status, &text).into())
}

/// The report for a failed response: the status, then its JSON
/// `error` field or else its trimmed body; the status alone when the
/// body is empty.
fn error_line(status: reqwest::StatusCode, text: &str) -> String {
    let msg = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| text.trim().to_owned());
    if msg.is_empty() {
        status.to_string()
    } else {
        format!("{status}: {msg}")
    }
}

/// Backslash-escape a string for embedding inside Lisp source.
/// Handles the two characters that break a `"…"` literal: `"`
/// and `\`. Deliberately NOT the library's `escape_lisp_string`:
/// that one also strips control characters (it writes files that
/// get re-evaled), which would silently delete the newlines in a
/// multi-line scenario-event payload sent from the CLI.
fn lisp_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// `latest[stream].value` as f64 from an `/api/.../microgrid/latest`
/// map — shared by the pool and dashboard line builders.
fn latest_value(latest: &serde_json::Value, stream: &str) -> Option<f64> {
    latest
        .get(stream)
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_f64())
}

fn print_summary(s: &serde_json::Value, json: bool) {
    if json {
        println!("{s:#}");
        return;
    }
    let name = s["name"].as_str().unwrap_or("(none)");
    let started = s["started_at"].as_str().unwrap_or("—");
    let ended = s["ended_at"].as_str().unwrap_or("running");
    let elapsed = s["elapsed_s"].as_f64().unwrap_or(0.0);
    let n = s["event_count"].as_u64().unwrap_or(0);
    println!("name        {name}");
    println!("started_at  {started}");
    println!("ended_at    {ended}");
    println!("elapsed     {elapsed:.1} s");
    println!("events      {n}");
}

fn print_report(r: &serde_json::Value, json: bool) {
    if json {
        println!("{r:#}");
        return;
    }
    fn kw(v: f64) -> String {
        format!("{:.2} kW", v / 1000.0)
    }
    fn kwh(v: f64) -> String {
        format!("{:.2} kWh", v / 1000.0)
    }
    let elapsed = r["scenario_elapsed_s"].as_f64().unwrap_or(0.0);
    let peak = r["peak_grid_w"].as_f64().unwrap_or(0.0);
    let chg = r["total_battery_charged_wh"].as_f64().unwrap_or(0.0);
    let dchg = r["total_battery_discharged_wh"].as_f64().unwrap_or(0.0);
    let pv = r["total_pv_produced_wh"].as_f64().unwrap_or(0.0);
    println!("elapsed              {elapsed:.1} s");
    let passed = r["checks_passed"].as_u64().unwrap_or(0);
    let failed = r["checks_failed"].as_u64().unwrap_or(0);
    if passed + failed > 0 {
        println!("checks               {passed} passed, {failed} failed");
        if let Some(arr) = r["checks"].as_array() {
            for c in arr.iter().filter(|c| c["passed"] != true) {
                let id = c["component_id"].as_u64().unwrap_or(0);
                let metric = c["metric"].as_str().unwrap_or("?");
                let expectation = c["expectation"].as_str().unwrap_or("?");
                let actual = c["actual"]
                    .as_f64()
                    .map(|v| format!("{v}"))
                    .unwrap_or_else(|| "unavailable".into());
                println!("  FAIL  {id} {metric}: expected {expectation}, actual {actual}");
            }
        }
    }
    println!("grid peak            {}", kw(peak));
    println!("battery charged      {}", kwh(chg));
    println!("battery discharged   {}", kwh(dchg));
    println!("PV produced          {}", kwh(pv));
    if let Some(soc) = r["soc_stats"].as_object() {
        let mean = soc["mean_pct"].as_f64().unwrap_or(0.0);
        let median = soc["median_pct"].as_f64().unwrap_or(0.0);
        let mode = soc["mode_pct"]
            .as_u64()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into());
        println!("SoC mean / median / mode  {mean:.1}% / {median:.1}% / {mode}%");
    }
    if let Some(arr) = r["grid_window_averages"].as_array()
        && !arr.is_empty()
    {
        println!("\n15-min grid averages (last 6):");
        for w in arr.iter().rev().take(6).collect::<Vec<_>>().iter().rev() {
            let ts = w["window_start"].as_str().unwrap_or("?");
            let avg = w["avg_w"].as_f64().unwrap_or(0.0);
            println!("  {ts}  {}", kw(avg));
        }
    }
    if let Some(arr) = r["per_battery"].as_array()
        && !arr.is_empty()
    {
        println!("\nper battery:");
        for b in arr {
            let id = b["id"].as_u64().unwrap_or(0);
            let c = b["charge_wh"].as_f64().unwrap_or(0.0);
            let d = b["discharge_wh"].as_f64().unwrap_or(0.0);
            println!("  {id}  charge {}  discharge {}", kwh(c), kwh(d));
        }
    }
}

fn print_events(e: &serde_json::Value, json: bool) {
    if json {
        println!("{e:#}");
        return;
    }
    let next = e["next_event_id"].as_u64().unwrap_or(0);
    if let Some(arr) = e["events"].as_array() {
        if arr.is_empty() {
            println!("(no events)");
        } else {
            println!("{:>5}  {:<24}  {:<14}  payload", "id", "ts", "kind");
            for ev in arr {
                let id = ev["id"].as_u64().unwrap_or(0);
                let ts = ev["ts"].as_str().unwrap_or("?");
                let kind = ev["kind"].as_str().unwrap_or("?");
                let payload = ev["payload"].as_str().unwrap_or("");
                println!("{id:>5}  {ts:<24}  {kind:<14}  {payload}");
            }
        }
    }
    println!("\nnext_event_id: {next}");
}

async fn cmd_info(
    client: &mut MicrogridClient<Channel>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = client.get_microgrid(()).await?.into_inner();
    if json {
        println!("{:#?}", resp);
        return Ok(());
    }
    if let Some(mg) = resp.microgrid {
        println!("microgrid_id   = {}", mg.id);
        println!("enterprise_id  = {}", mg.enterprise_id);
        println!("name           = {}", mg.name);
        println!("status         = {}", mg.status);
        if let Some(t) = mg.create_timestamp {
            println!("created_at     = {}", format_ts(&t));
        }
    } else {
        println!("(no microgrid)");
    }
    Ok(())
}

async fn cmd_list(
    client: &mut MicrogridClient<Channel>,
    category: Option<Category>,
    ids: Vec<u64>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = ListElectricalComponentsRequest {
        electrical_component_categories: category
            .map(|c| vec![c.to_proto() as i32])
            .unwrap_or_default(),
        electrical_component_ids: ids,
    };
    let resp = client.list_electrical_components(req).await?.into_inner();
    if json {
        println!("{:#?}", resp.electrical_components);
        return Ok(());
    }
    println!(
        "{:>5}  {:<24}  {:<10}  {:<10}  {:>12}  {:>12}",
        "id", "name", "category", "subtype", "rated_lower", "rated_upper"
    );
    for c in &resp.electrical_components {
        let (lo, hi) = active_bounds(c);
        println!(
            "{:>5}  {:<24}  {:<10}  {:<10}  {:>12}  {:>12}",
            c.id,
            c.name,
            short_category(c.category),
            short_subtype(c),
            fmt_opt(lo),
            fmt_opt(hi),
        );
    }
    Ok(())
}

async fn cmd_connections(
    client: &mut MicrogridClient<Channel>,
    from: Vec<u64>,
    to: Vec<u64>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = ListElectricalComponentConnectionsRequest {
        source_electrical_component_ids: from,
        destination_electrical_component_ids: to,
    };
    let resp = client
        .list_electrical_component_connections(req)
        .await?
        .into_inner();
    if json {
        println!("{:#?}", resp.electrical_component_connections);
        return Ok(());
    }
    for c in &resp.electrical_component_connections {
        println!(
            "{} -> {}",
            c.source_electrical_component_id, c.destination_electrical_component_id
        );
    }
    Ok(())
}

async fn cmd_tree(client: &mut MicrogridClient<Channel>) -> Result<(), Box<dyn std::error::Error>> {
    let comps = client
        .list_electrical_components(ListElectricalComponentsRequest::default())
        .await?
        .into_inner()
        .electrical_components;
    let conns = client
        .list_electrical_component_connections(ListElectricalComponentConnectionsRequest::default())
        .await?
        .into_inner()
        .electrical_component_connections;

    let by_id: BTreeMap<u64, &ElectricalComponent> = comps.iter().map(|c| (c.id, c)).collect();
    let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    let mut has_parent: BTreeSet<u64> = BTreeSet::new();
    for c in &conns {
        children
            .entry(c.source_electrical_component_id)
            .or_default()
            .push(c.destination_electrical_component_id);
        has_parent.insert(c.destination_electrical_component_id);
    }

    let roots: Vec<u64> = by_id
        .keys()
        .copied()
        .filter(|id| !has_parent.contains(id))
        .collect();
    for (i, r) in roots.iter().enumerate() {
        print_tree(*r, &by_id, &children, "", i == roots.len() - 1, true);
    }
    Ok(())
}

fn print_tree(
    id: u64,
    by_id: &BTreeMap<u64, &ElectricalComponent>,
    children: &BTreeMap<u64, Vec<u64>>,
    prefix: &str,
    is_last: bool,
    is_root: bool,
) {
    let connector = if is_root {
        ""
    } else if is_last {
        "└── "
    } else {
        "├── "
    };
    let label = match by_id.get(&id) {
        Some(c) => format!("[{}] {} ({})", c.id, c.name, short_category(c.category)),
        None => format!("[{id}] <unknown>"),
    };
    println!("{prefix}{connector}{label}");
    if let Some(kids) = children.get(&id) {
        let new_prefix = if is_root {
            String::new()
        } else if is_last {
            format!("{prefix}    ")
        } else {
            format!("{prefix}│   ")
        };
        for (i, k) in kids.iter().enumerate() {
            print_tree(*k, by_id, children, &new_prefix, i == kids.len() - 1, false);
        }
    }
}

async fn cmd_stream(
    client: &mut MicrogridClient<Channel>,
    id: u64,
    samples: Option<usize>,
    metric_names: Vec<String>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let filter = if metric_names.is_empty() {
        None
    } else {
        let mut metrics = Vec::with_capacity(metric_names.len());
        for name in &metric_names {
            metrics.push(parse_metric_name(name)? as i32);
        }
        Some(
            macrocosim::proto::microgrid::receive_electrical_component_telemetry_stream_request::ComponentTelemetryStreamFilter {
                metrics,
            },
        )
    };
    let req = ReceiveElectricalComponentTelemetryStreamRequest {
        electrical_component_id: id,
        filter,
    };
    let mut stream = client
        .receive_electrical_component_telemetry_stream(req)
        .await?
        .into_inner();

    if !json {
        println!(
            "{:<24}  {:<26}  {:>14}  {:<22}",
            "time", "metric", "value", "bounds"
        );
    }

    let mut got = 0usize;
    while samples != Some(got)
        && let Some(msg) = stream.message().await?
    {
        let Some(t) = msg.telemetry else { continue };
        if json {
            println!("{:#?}", t);
        } else {
            for s in &t.metric_samples {
                let metric = Metric::try_from(s.metric)
                    .map(short_metric)
                    .unwrap_or_else(|_| format!("METRIC_{}", s.metric));
                let value = s
                    .value
                    .as_ref()
                    .and_then(|v| v.metric_value_variant.as_ref())
                    .and_then(|v| match v {
                        macrocosim::proto::common::metrics::metric_value_variant::MetricValueVariant::SimpleMetric(sv) => Some(sv.value),
                        _ => None,
                    });
                let bounds = if s.bounds.is_empty() {
                    String::new()
                } else {
                    s.bounds
                        .iter()
                        .map(|b| format!("[{}, {}]", fmt_opt(b.lower), fmt_opt(b.upper)))
                        .collect::<Vec<_>>()
                        .join(",")
                };
                let ts = s
                    .sample_time
                    .as_ref()
                    .map(format_ts)
                    .unwrap_or_else(|| "-".into());
                let val_str = match value {
                    Some(v) => format!("{:>14.2}", v),
                    None => format!("{:>14}", "-"),
                };
                println!("{ts:<24}  {metric:<26}  {val_str}  {bounds}");
            }
        }
        got += 1;
    }
    Ok(())
}

async fn cmd_set_power(
    client: &mut MicrogridClient<Channel>,
    id: u64,
    power: f32,
    reactive: bool,
    lifetime: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = SetElectricalComponentPowerRequest {
        electrical_component_id: id,
        power,
        power_type: if reactive {
            PowerType::Reactive as i32
        } else {
            PowerType::Active as i32
        },
        request_lifetime: lifetime,
    };
    let mut stream = client
        .set_electrical_component_power(req)
        .await?
        .into_inner();
    while let Some(msg) = stream.message().await? {
        let name =
            macrocosim::proto::microgrid::SetElectricalComponentPowerRequestStatus::try_from(
                msg.status,
            )
            .map(|s| {
                s.as_str_name()
                    .trim_start_matches("SET_ELECTRICAL_COMPONENT_POWER_REQUEST_STATUS_")
                    .to_string()
            })
            .unwrap_or_else(|_| msg.status.to_string());
        println!("status: {name}");
        if let Some(t) = msg.valid_until_time {
            println!("valid_until: {}", format_ts(&t));
        }
    }
    Ok(())
}

async fn cmd_augment(
    client: &mut MicrogridClient<Channel>,
    id: u64,
    lower: f32,
    upper: f32,
    reactive: bool,
    lifetime: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    use macrocosim::proto::common::metrics::Bounds;
    let target_metric = if reactive {
        Metric::AcPowerReactive
    } else {
        Metric::AcPowerActive
    };
    let req = AugmentElectricalComponentBoundsRequest {
        electrical_component_id: id,
        target_metric: target_metric as i32,
        bounds: vec![Bounds {
            lower: Some(lower),
            upper: Some(upper),
        }],
        request_lifetime: Some(lifetime),
    };
    let resp = client
        .augment_electrical_component_bounds(req)
        .await?
        .into_inner();
    if let Some(t) = resp.valid_until_time {
        println!("valid_until: {}", format_ts(&t));
    } else {
        println!("(no expiry returned)");
    }
    Ok(())
}

// ---------- formatting helpers --------------------------------------------

fn fmt_opt(v: Option<f32>) -> String {
    v.map(|x| format!("{x}")).unwrap_or_else(|| "*".into())
}

fn format_ts(t: &prost_types::Timestamp) -> String {
    use chrono::{DateTime, Utc};
    let secs = t.seconds;
    let nanos = t.nanos.max(0) as u32;
    DateTime::<Utc>::from_timestamp(secs, nanos)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| format!("{secs}.{nanos:09}"))
}

fn short_category(cat: i32) -> &'static str {
    match ElectricalComponentCategory::try_from(cat) {
        Ok(ElectricalComponentCategory::GridConnectionPoint) => "grid",
        Ok(ElectricalComponentCategory::Meter) => "meter",
        Ok(ElectricalComponentCategory::Inverter) => "inverter",
        Ok(ElectricalComponentCategory::Battery) => "battery",
        Ok(ElectricalComponentCategory::EvCharger) => "ev",
        Ok(ElectricalComponentCategory::Chp) => "chp",
        Ok(ElectricalComponentCategory::SteamBoiler) => "boiler",
        _ => "?",
    }
}

fn short_subtype(c: &ElectricalComponent) -> String {
    let Some(info) = c.category_specific_info.as_ref() else {
        return String::new();
    };
    let Some(kind) = info.kind.as_ref() else {
        return String::new();
    };
    match kind {
        Kind::Inverter(i) => format!("{:?}", i.r#type()),
        Kind::Battery(b) => format!("{:?}", b.r#type()),
        Kind::EvCharger(e) => format!("{:?}", e.r#type()),
        Kind::GridConnectionPoint(g) => format!("fuse={}", g.rated_fuse_current),
        _ => String::new(),
    }
}

fn active_bounds(c: &ElectricalComponent) -> (Option<f32>, Option<f32>) {
    for b in &c.metric_config_bounds {
        let metric = Metric::try_from(b.metric).ok();
        if matches!(metric, Some(Metric::AcPowerActive) | Some(Metric::DcPower))
            && let Some(cb) = &b.config_bounds
        {
            return (cb.lower, cb.upper);
        }
    }
    (None, None)
}

fn short_metric(m: Metric) -> String {
    m.as_str_name().trim_start_matches("METRIC_").to_string()
}

/// Resolve a CLI string to the proto `Metric` enum. Accepts both
/// `METRIC_AC_POWER_ACTIVE` and the lowercase `ac_power_active` /
/// `ac-power-active` shorthands; the lookup is case-insensitive and
/// tolerant of `-` vs `_` so users don't have to fight tab-completion.
fn parse_metric_name(s: &str) -> Result<Metric, Box<dyn std::error::Error>> {
    let normalized = s.replace('-', "_").to_ascii_uppercase();
    let with_prefix = if normalized.starts_with("METRIC_") {
        normalized
    } else {
        format!("METRIC_{normalized}")
    };
    Metric::from_str_name(&with_prefix).ok_or_else(|| format!("unknown metric '{s}'").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_line_prefers_the_error_field() {
        use reqwest::StatusCode;
        assert_eq!(
            error_line(
                StatusCode::NOT_FOUND,
                r#"{"error":"microgrid 9 not registered"}"#
            ),
            "404 Not Found: microgrid 9 not registered"
        );
        assert_eq!(error_line(StatusCode::BAD_GATEWAY, ""), "502 Bad Gateway");
        assert_eq!(
            error_line(StatusCode::BAD_GATEWAY, "  \n"),
            "502 Bad Gateway"
        );
        assert_eq!(
            error_line(StatusCode::BAD_GATEWAY, "oops\n"),
            "502 Bad Gateway: oops"
        );
    }
}
