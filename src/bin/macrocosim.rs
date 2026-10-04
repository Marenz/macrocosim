//! Headless macrocosim simulator: load `config.lisp`, spawn the
//! physics tick, serve the Microgrid gRPC API.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;
use macrocosim::{
    assets_server::AssetsServer, dispatch_server::DispatchServer, lisp::Config,
    proto::assets::platform_assets_server::PlatformAssetsServer as AssetsGrpcServer,
    proto::dispatch::microgrid_dispatch_service_server::MicrogridDispatchServiceServer as DispatchGrpcServer,
    proto::microgrid::microgrid_server::MicrogridServer as MicrogridGrpcServer,
    server::MicrogridServer, sim::MicrogridSite, ui, ui_log,
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

    // Snapshot the enterprise registry: one tuple per microgrid
    // (id, name, grpc_port, site). Each will get its own physics
    // tick + history sampler + Microgrid gRPC server.
    let entries: Vec<(u64, String, u16, macrocosim::sim::MicrogridSite)> = config
        .microgrids()
        .lock()
        .values()
        .map(|e| {
            (
                e.def.id,
                e.def.name.clone(),
                e.def.grpc_port,
                e.site.clone(),
            )
        })
        .collect();
    log::info!(
        "Enterprise carries {} microgrid(s); spawning per-microgrid runtimes",
        entries.len()
    );
    for (id, name, port, site) in &entries {
        log::info!(
            "Microgrid #{id} {name:?} → :{port} ({} components, {} connections)",
            site.components().len(),
            site.connections().len(),
        );
        MicrogridSite::clone(site).spawn_physics();
        MicrogridSite::clone(site).spawn_history_sampler();
    }

    // Watch the config file in the background so saves trigger reload.
    tokio::spawn(config.clone().watch());

    // Bind every listener up front (synchronously) so ephemeral (:0)
    // ports resolve to real ones before we wire loopbacks + emit the
    // endpoints. Hosts stay loopback (UI 127.0.0.1, gRPC [::1]); a
    // routable --*-bind + the hardening it gates is a follow-up
    // (todo §D3).
    let eph = args.ephemeral_ports;
    let ui_port = if eph { 0 } else { args.ui_port };
    let (ui_listener, ui_addr) =
        bind_or_exit(SocketAddr::from((Ipv4Addr::LOCALHOST, ui_port)), "UI").await;
    let ui_config = config.clone();

    // Per-microgrid gRPC listeners: (id, name, site, listener, addr).
    let mut bound: Vec<(
        u64,
        String,
        MicrogridSite,
        tokio::net::TcpListener,
        SocketAddr,
    )> = Vec::with_capacity(entries.len());
    for (id, name, port, site) in entries {
        let port = if eph { 0 } else { port };
        let (listener, addr) = bind_or_exit(
            SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
            &format!("Microgrid #{id} gRPC"),
        )
        .await;
        bound.push((id, name, site, listener, addr));
    }
    let boot_ids: Vec<u64> = bound.iter().map(|b| b.0).collect();

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

    // One loopback Microgrid client per microgrid, pointed at the
    // *resolved* gRPC address. Keyed by id so /api/mg/{id}/microgrid/*
    // looks up the right slot; the legacy /api/microgrid/* endpoints
    // read the *first* microgrid's slot for backward compat.
    let loopbacks = ui::new_microgrid_loopbacks();
    let first_id = bound.first().map(|b| b.0);
    let mut primary_slot: Option<ui::SharedMicrogrid> = None;
    for (id, name, site, _listener, addr) in &bound {
        let slot = ui::new_microgrid_slot();
        ui::spawn_microgrid_loopback(format!("http://{addr}"), slot.clone(), site.clone());
        loopbacks.write().insert(*id, slot.clone());
        if Some(*id) == first_id {
            primary_slot = Some(slot);
        }
        log::info!("Microgrid #{id} {name:?} loopback client spawned");
    }
    // Bare boot: the legacy /api/microgrid/* endpoints read this
    // slot; with no boot-time microgrids it stays an empty,
    // never-connected slot (the per-mg routes serve runtime loads).
    let microgrid = primary_slot.unwrap_or_else(ui::new_microgrid_slot);

    // Runtime-create callback: when POST /api/microgrids/create
    // inserts a new entry into the registry, this closure spawns
    // its physics tick + history sampler + Microgrid gRPC server
    // (on the assigned port) + loopback client. Cloning Arcs
    // captures the runtime state we need; the closure itself is
    // Send + Sync so it can ride through an axum Extension.
    let spawner_config = config.clone();
    let spawner_loopbacks = loopbacks.clone();
    let spawner_eph = eph;
    let spawner: ui::MicrogridSpawner = std::sync::Arc::new(move |id, name, port, site| {
        site.clone().spawn_physics();
        site.clone().spawn_history_sampler();
        // Honor --ephemeral-ports here too: bind :0 for an OS-chosen
        // port so a runtime-created microgrid doesn't clash with a
        // parallel instance on its config-declared port. Bind up front
        // (std → tokio) so the loopback client + log use the resolved
        // port. (Runtime-created microgrids are still absent from the
        // --emit-endpoints readiness signal — see the emit below.)
        let bind_port = if spawner_eph { 0 } else { port };
        let listener = match std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, bind_port))
            .and_then(|l| l.set_nonblocking(true).map(|()| l))
            .and_then(tokio::net::TcpListener::from_std)
        {
            Ok(l) => l,
            Err(e) => {
                log::error!(
                    "Microgrid #{id} {name:?} create: gRPC bind [::1]:{bind_port} failed ({e}); skipping"
                );
                return;
            }
        };
        let addr = listener.local_addr().expect("resolved gRPC addr");
        let cfg = spawner_config.clone();
        let site_for_server = site.clone();
        let name_owned = name.to_string();
        tokio::spawn(async move {
            log::info!("Microgrid #{id} {name_owned:?} runtime-created → gRPC {addr}");
            let server = MicrogridServer::new(cfg, id, site_for_server);
            if let Err(e) = Server::builder()
                .add_service(MicrogridGrpcServer::new(server))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
            {
                log::error!("Microgrid #{id} gRPC server exited: {e}");
            }
        });
        let slot = ui::new_microgrid_slot();
        ui::spawn_microgrid_loopback(format!("http://{addr}"), slot.clone(), site);
        spawner_loopbacks.write().insert(id, slot);
    });

    // Single spawn path for microgrids registered after boot. A
    // `(make-microgrid …)` evaluated at runtime — REPL eval, a config
    // reload that added an entry, or the create-microgrid HTTP
    // endpoint (which only notifies; see handlers/microgrids.rs) —
    // broadcasts on the registered channel, and this listener boots
    // the same runtime set the boot loop below gives boot-time
    // entries. Reused (reload) registrations notify too and the
    // `spawned` set drops duplicates, so no path double-boots a
    // runtime.
    {
        let config = config.clone();
        let spawner = spawner.clone();
        let mut spawned: std::collections::HashSet<u64> = boot_ids.iter().copied().collect();
        tokio::spawn(async move {
            let mut rx = config.subscribe_microgrid_registered();
            loop {
                let ids: Vec<u64> = match rx.recv().await {
                    Ok(id) => vec![id],
                    // Fell behind a registration burst — the per-id
                    // notifications in the gap are lost, so re-snapshot
                    // the registry and boot anything unseen.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("microgrid spawner lagged {n} registrations; re-snapshotting");
                        config.microgrids().lock().keys().copied().collect()
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                for id in ids {
                    if !spawned.insert(id) {
                        continue;
                    }
                    let entry = config.microgrids().lock().get(&id).cloned();
                    match entry {
                        Some(e) => spawner(e.def.id, &e.def.name, e.def.grpc_port, e.site),
                        None => {
                            // Registered then removed before we looked —
                            // forget it so a later re-registration spawns.
                            log::warn!("microgrid_registered({id}) but registry has no entry");
                            spawned.remove(&id);
                        }
                    }
                }
            }
        });
    }

    // Emit the resolved endpoints once everything is bound — the
    // machine-readable readiness signal. Boot-time microgrids only;
    // runtime-created ones (POST /api/microgrids/create) aren't listed.
    if let Some(target) = &args.emit_endpoints {
        let json = serde_json::json!({
            "ui": ui_addr.to_string(),
            "microgrids": bound
                .iter()
                .map(|(id, name, _, _, addr)| {
                    serde_json::json!({ "id": id, "name": name, "grpc": addr.to_string() })
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

    // Critical long-running tasks (UI server, every gRPC listener)
    // go into one JoinSet: any of them exiting means the process is
    // limping with a dead surface, so main notices the FIRST exit and
    // shuts the whole binary down instead of serving degraded. (The
    // lisp refresh + timeout loops live inside Config and stay
    // fire-and-forget for now.)
    let mut tasks: tokio::task::JoinSet<&'static str> = tokio::task::JoinSet::new();
    log::info!("Macrocosim UI listening on http://{ui_addr}");
    tasks.spawn(async move {
        if let Err(e) = ui::serve_with_listener(ui_listener, ui_config, microgrid, loopbacks).await
        {
            log::error!("UI server exited: {e}");
        }
        "UI server"
    });

    // One Microgrid gRPC server per registry entry, each driving its
    // pre-bound listener (serve_with_incoming, so the served port is
    // exactly the one we resolved + reported above).
    for (id, name, site, listener, addr) in bound {
        log::info!("Microgrid #{id} {name:?} gRPC listening on {addr}");
        let cfg_for_server = config.clone();
        tasks.spawn(async move {
            let mg_server = MicrogridServer::new(cfg_for_server, id, site);
            if let Err(e) = Server::builder()
                .add_service(MicrogridGrpcServer::new(mg_server))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
            {
                log::error!("Microgrid #{id} gRPC server exited: {e}");
            }
            "Microgrid gRPC server"
        });
    }
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
