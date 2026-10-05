//! Headless macrocosim simulator: load `config.lisp`, spawn the
//! physics tick, serve the Microgrid gRPC API.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;
use macrocosim::{
    assets_server::AssetsServer,
    dispatch_server::DispatchServer,
    lisp::Config,
    proto::assets::platform_assets_server::PlatformAssetsServer as AssetsGrpcServer,
    proto::dispatch::microgrid_dispatch_service_server::MicrogridDispatchServiceServer as DispatchGrpcServer,
    runtime::{MicrogridRuntimes, RuntimeOptions, RuntimeStatus},
    ui, ui_log,
};
use simplelog::{
    ColorChoice, CombinedLogger, ConfigBuilder, LevelFilter, TermLogger, TerminalMode,
};
use tonic::transport::Server;

use tokio_stream::wrappers::TcpListenerStream;

/// Headless macrocosim microgrid simulator.
#[derive(Parser)]
struct Args {
    /// Lisp scripts to evaluate at boot, in order. With none, the
    /// engine boots bare: UI + REPL up, empty registry — load a
    /// topology on demand with `(load "…")` from the REPL or the
    /// Microgrids tab.
    scripts: Vec<PathBuf>,

    /// Anchor directory for persistent state (enterprise.lisp,
    /// snapshots/, managed microgrid files) and for relative
    /// `(load …)` paths. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,

    /// UI HTTP port (0 = OS-chosen). Ignored under --ephemeral-ports.
    #[arg(long, default_value_t = 8801)]
    ui_port: u16,

    /// Bind the UI and every gRPC listener (per-microgrid, assets,
    /// dispatch) on an OS-chosen port, overriding config / defaults —
    /// for running parallel instances (e.g. CI) without port clashes.
    #[arg(long)]
    ephemeral_ports: bool,

    /// Once every listener is bound, write the resolved endpoints as
    /// one JSON line — to `--emit-endpoints=PATH`, or stdout if the flag
    /// is given bare. Requires `=` so it can't swallow the `config`
    /// positional. The machine-readable readiness signal.
    #[arg(long, value_name = "PATH", num_args = 0..=1, require_equals = true, default_missing_value = "-")]
    emit_endpoints: Option<String>,
}

/// Bind a TCP listener for `label`, or log and exit the process on
/// failure. Returns the listener and its resolved local address — the
/// OS-chosen port when `addr`'s port is 0 (`--ephemeral-ports`).
async fn bind_or_exit(addr: SocketAddr, label: &str) -> (tokio::net::TcpListener, SocketAddr) {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| {
            log::error!("{label} bind {addr} failed: {e}");
            std::process::exit(1);
        });
    let resolved = listener.local_addr().unwrap();
    (listener, resolved)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = Args::parse();

    // Suppress per-tick "channel closed" spam from frequenz-microgrid
    // 0.4.1's ComponentTelemetryTracker. When a `BatteryPool` drops
    // (which happens on every topology rebuild) the tracker tasks it
    // spawned keep ticking on a timer and log at error level when
    // they fail to send into the closed mpsc — see
    // /vagrant/upstream-tracker-leak.md. The trackers are otherwise
    // harmless (orphaned, no measurable CPU), but the log spam scales
    // linearly with rebuilds. Drop the noisy module here — same list
    // applied to both the terminal logger and the UI tap so the SPA's
    // log panel + /api/logs backfill stay clean too.
    let ignore_targets: &[&str] =
        &["frequenz_microgrid::microgrid::telemetry_tracker::component_telemetry_tracker"];

    // Combined logger: terminal output (existing UX) + a tap that
    // captures records into a ring buffer + broadcasts them on a
    // tokio channel. The UI server reads both: /api/logs returns the
    // ring for backfill on page load, /ws/events forwards the live
    // stream so the SPA's log panel updates in real time.
    let log_tap = ui_log::LogTap::new(
        500,
        LevelFilter::Info,
        ignore_targets.iter().map(|s| (*s).to_owned()).collect(),
    );
    ui_log::LOG_TAP
        .set(log_tap.clone())
        .unwrap_or_else(|_| panic!("LOG_TAP already initialised"));
    let mut log_cfg = ConfigBuilder::new();
    for t in ignore_targets {
        log_cfg.add_filter_ignore_str(t);
    }
    let log_config = log_cfg.build();
    CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Info,
            log_config,
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        Box::new(log_tap),
    ])
    .unwrap();

    let scripts: Vec<String> = args
        .scripts
        .iter()
        .map(|p| {
            p.to_str().map(str::to_owned).unwrap_or_else(|| {
                log::error!("Script path is not valid UTF-8: {}", p.display());
                std::process::exit(1);
            })
        })
        .collect();
    if scripts.is_empty() {
        log::info!("No boot scripts given — starting a bare engine");
    } else {
        log::info!("Evaluating boot script(s): {}", scripts.join(", "));
    }
    let config = Config::new_with(&scripts, args.state_dir.clone()).unwrap_or_else(|e| {
        log::error!("Failed to eval boot scripts:\n{e}");
        std::process::exit(1);
    });

    // Every microgrid runtime, boot-time and later, starts through
    // MicrogridRuntimes. The listener subscribes before the file
    // watcher starts, so no reload can register a microgrid unseen.
    let runtimes = MicrogridRuntimes::new(
        config.clone(),
        RuntimeOptions {
            ephemeral_ports: args.ephemeral_ports,
            bind_host: Ipv6Addr::LOCALHOST.into(),
        },
    )
    .unwrap_or_else(|e| {
        log::error!("{e}");
        std::process::exit(1);
    });
    let _listener = runtimes.spawn_registration_listener().await;
    let boot_ids: Vec<u64> = config.microgrids().lock().keys().copied().collect();
    log::info!("Enterprise carries {} microgrid(s)", boot_ids.len());
    // `--emit-endpoints` promises every boot listener is up, so a
    // boot microgrid that failed to start exits the process.
    for id in &boot_ids {
        if let Some(v) = runtimes.status(*id)
            && v.status == RuntimeStatus::Failed
        {
            log::error!("Microgrid #{id} failed to start; exiting");
            std::process::exit(1);
        }
    }

    // Watch the config file in the background so saves trigger reload.
    tokio::spawn(config.clone().watch());

    // Bind the UI, assets and dispatch listeners up front so
    // ephemeral (:0) ports resolve to real ones before the endpoints
    // are emitted. Hosts stay loopback (UI 127.0.0.1, gRPC [::1]); a
    // routable --*-bind + the hardening it gates is a follow-up
    // (todo §D3).
    let eph = args.ephemeral_ports;
    let ui_port = if eph { 0 } else { args.ui_port };
    let (ui_listener, ui_addr) =
        bind_or_exit(SocketAddr::from((Ipv4Addr::LOCALHOST, ui_port)), "UI").await;
    let ui_config = config.clone();

    // Assets + dispatch: single enterprise-wide sockets (defaults
    // [::1]:9900 / [::1]:8900, lisp-overridable). --ephemeral-ports
    // zeroes the port for an OS-chosen one.
    let mut assets_addr: SocketAddr = config.assets_socket_addr().parse().unwrap_or_else(|e| {
        log::error!("invalid assets socket addr: {e}");
        std::process::exit(1);
    });
    if eph {
        assets_addr.set_port(0);
    }
    let (assets_listener, assets_addr) = bind_or_exit(assets_addr, "PlatformAssets").await;
    let mut dispatch_addr: SocketAddr = config.dispatch_socket_addr().parse().unwrap_or_else(|e| {
        log::error!("invalid dispatch socket addr: {e}");
        std::process::exit(1);
    });
    if eph {
        dispatch_addr.set_port(0);
    }
    let (dispatch_listener, dispatch_addr) = bind_or_exit(dispatch_addr, "MicrogridDispatch").await;

    // The legacy /api/microgrid/* endpoints read the first boot
    // microgrid's slot; on a bare boot it is an empty,
    // never-connected slot (the per-mg routes serve runtime loads).
    let microgrid = boot_ids
        .first()
        .and_then(|id| runtimes.loopbacks().read().get(id).cloned())
        .unwrap_or_else(ui::new_microgrid_slot);

    // Emit the resolved endpoints once everything is bound — the
    // machine-readable readiness signal. Boot-time microgrids only;
    // runtime-created ones (POST /api/microgrids/create) aren't listed.
    if let Some(target) = &args.emit_endpoints {
        let json = serde_json::json!({
            "ui": ui_addr.to_string(),
            "microgrids": boot_ids
                .iter()
                .filter_map(|id| {
                    let name = config.microgrids().lock().get(id)?.def.name.clone();
                    let addr = runtimes.status(*id)?.grpc_addr?;
                    Some(serde_json::json!({ "id": id, "name": name, "grpc": addr }))
                })
                .collect::<Vec<_>>(),
            "assets": assets_addr.to_string(),
            "dispatch": dispatch_addr.to_string(),
        })
        .to_string();
        if target == "-" {
            println!("{json}");
        } else {
            // This file is the readiness signal a harness polls for,
            // so a failed write must kill the process (like a failed
            // bind) — not leave a healthy-looking server the poller
            // can never detect. Write + rename so the file appears
            // only once its content is complete.
            let tmp = format!("{target}.tmp");
            if let Err(e) = std::fs::write(&tmp, format!("{json}\n"))
                .and_then(|()| std::fs::rename(&tmp, target))
            {
                log::error!("emit-endpoints write {target}: {e}");
                std::process::exit(1);
            }
        }
    }

    // Critical long-running tasks (UI, PlatformAssets and dispatch
    // servers) go into one JoinSet: any of them exiting means the
    // process is limping with a dead surface, so main notices the
    // FIRST exit and shuts the whole binary down instead of serving
    // degraded. A microgrid's gRPC server reports through its runtime
    // status instead. (The lisp refresh loop lives inside Config and
    // stays fire-and-forget for now.)
    let mut tasks: tokio::task::JoinSet<&'static str> = tokio::task::JoinSet::new();
    log::info!("Macrocosim UI listening on http://{ui_addr}");
    tasks.spawn(async move {
        if let Err(e) =
            ui::serve_with_listener(ui_listener, ui_config, microgrid, runtimes.clone()).await
        {
            log::error!("UI server exited: {e}");
        }
        "UI server"
    });

    // PlatformAssets — its own listener, reachable regardless of which
    // microgrid the client picks.
    log::info!("PlatformAssets gRPC listening on {assets_addr}");
    let cfg_for_assets = config.clone();
    tasks.spawn(async move {
        if let Err(e) = Server::builder()
            .add_service(AssetsGrpcServer::new(AssetsServer::new(cfg_for_assets)))
            .serve_with_incoming(TcpListenerStream::new(assets_listener))
            .await
        {
            log::error!("PlatformAssets gRPC server exited: {e}");
        }
        "PlatformAssets gRPC server"
    });
    // The single (enterprise-wide) MicrogridDispatchService — its own
    // listener, one service fronting every microgrid (keyed by the
    // microgrid_id carried in each request).
    log::info!("MicrogridDispatch gRPC listening on {dispatch_addr}");
    let dispatch_store = config.dispatches();
    let dispatch_registry = config.microgrids();
    tasks.spawn(async move {
        if let Err(e) = Server::builder()
            .add_service(DispatchGrpcServer::new(DispatchServer::new(
                dispatch_store,
                dispatch_registry,
            )))
            .serve_with_incoming(TcpListenerStream::new(dispatch_listener))
            .await
        {
            log::error!("MicrogridDispatch gRPC server exited: {e}");
        }
        "MicrogridDispatch gRPC server"
    });
    // First exit wins: a critical surface died (its own error was
    // already logged), so stop the whole process rather than limping
    // on with the remaining listeners.
    if let Some(res) = tasks.join_next().await {
        match res {
            Ok(label) => log::error!("{label} exited; shutting down"),
            Err(e) => log::error!("critical task panicked: {e}; shutting down"),
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare `macrocosim some.lisp` invocation keeps the documented
    /// defaults: UI on 8801, no state dir, the script positional.
    /// CI always boots with --ephemeral-ports and --state-dir, so
    /// nothing else exercises these defaults.
    #[test]
    fn bare_invocation_keeps_the_documented_defaults() {
        let a = Args::parse_from(["macrocosim", "some.lisp"]);
        assert_eq!(a.ui_port, 8801);
        assert!(a.state_dir.is_none());
        assert!(!a.ephemeral_ports);
        assert_eq!(a.scripts, vec![std::path::PathBuf::from("some.lisp")]);
    }
}
