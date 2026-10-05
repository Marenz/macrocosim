//! WebSocket push channel for the SPA.
//!
//! `/ws/events` upgrades to a long-lived socket; `event_pump`
//! subscribes to every microgrid's `SiteEvent` broadcast and
//! fans them out as JSON wrapped in a per-event mg_id tag.
//! Enterprise-scoped events (terminal log lines) ride without
//! a tag.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use serde::Serialize;

use crate::lisp::Config;
use crate::sim::events::SiteEvent;

/// WebSocket event push. Subscribers receive SiteEvent JSON for
/// every TopologyChanged + Sample broadcast. Client-sent frames are
/// drained but ignored — the channel is server-push only for v1; an
/// upcoming change adds a /api/eval-style RPC over the same socket
/// if it turns out latency-sensitive client actions benefit from it.
pub(super) async fn events_ws(
    ws: WebSocketUpgrade,
    State(config): State<Config>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| event_pump(socket, config))
}

/// Wrap a SiteEvent with the originating microgrid id so the SPA
/// can filter samples / topology bumps / setpoint events by the
/// currently-active microgrid. `mg_id` is `None` for enterprise-
/// scoped events (terminal log lines, ws lag notices).
#[derive(Serialize)]
struct WireEvent<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    mg_id: Option<u64>,
    #[serde(flatten)]
    event: &'a SiteEvent,
}

async fn event_pump(mut socket: WebSocket, config: Config) {
    use tokio::sync::broadcast::error::RecvError as BroadcastRecv;
    let mut log_rx = crate::ui_log::LOG_TAP.get().map(|t| t.subscribe());
    // Enterprise-wide dispatch lifecycle changes. Wrapped in Option so
    // a Closed (store dropped — shouldn't happen, it lives on Config)
    // parks the branch instead of busy-looping, mirroring `log_rx`.
    let mut dispatch_rx = Some(config.dispatches().subscribe());
    // Subscribe to every microgrid's per-site event bus + the
    // enterprise-wide `microgrid_registered` channel. The initial
    // snapshot covers every entry present at connect time; the
    // registered-channel branch in the select! below spawns a fresh
    // forwarder when POST /api/microgrids or (make-microgrid)
    // adds an entry mid-session. One forwarder task per site tags
    // events with the originating mg_id and pushes onto a shared
    // mpsc that the select! drains into the WebSocket.
    let (fwd_tx, mut fwd_rx) = tokio::sync::mpsc::channel::<(u64, SiteEvent)>(512);
    fn spawn_forwarder(
        mg_id: u64,
        mut rx: tokio::sync::broadcast::Receiver<SiteEvent>,
        tx: tokio::sync::mpsc::Sender<(u64, SiteEvent)>,
    ) -> tokio::task::JoinHandle<()> {
        use tokio::sync::broadcast::error::RecvError;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if tx.send((mg_id, ev)).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        log::warn!("ws: microgrid {mg_id} event bus lagged {n} samples");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        })
    }
    // Track every mg id we've already spawned a forwarder for, so a
    // Lagged on the registered channel can re-snapshot the registry
    // and back-fill forwarders for any entries that landed during
    // the gap (mass-create burst) without duplicating live ones.
    let mut subscribed_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // Forwarder handles, aborted when the socket closes — without
    // that, a forwarder for an idle microgrid parks until that
    // site's NEXT event before noticing the closed mpsc and exiting.
    let mut forwarders: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    // Subscribe BEFORE the registry snapshot below: a microgrid
    // registered between the two then shows up either in the
    // snapshot or on this receiver — never in neither. The
    // subscribed_ids dedup makes the overlap safe. Wrapped in an
    // Option so the select arm can park cleanly after Closed.
    let mut registered_rx = Some(config.subscribe_microgrid_registered());
    {
        let reg = config.microgrids();
        let r = reg.lock();
        for (id, entry) in r.iter() {
            forwarders.push(spawn_forwarder(
                *id,
                entry.site.subscribe_events(),
                fwd_tx.clone(),
            ));
            subscribed_ids.insert(*id);
        }
    }
    // Keep one clone of the fwd_tx alive on this task so fwd_rx
    // stays open across registration bursts — the per-mg forwarders
    // each hold their own clone, but a window of "no microgrids yet"
    // would otherwise close the mpsc and kill the loop.
    let fwd_tx_keepalive = fwd_tx.clone();
    drop(fwd_tx);
    loop {
        tokio::select! {
            ev = fwd_rx.recv() => match ev {
                Some((mg_id, event)) => {
                    let wire = WireEvent { mg_id: Some(mg_id), event: &event };
                    let json = match serde_json::to_string(&wire) {
                        Ok(j) => j,
                        Err(e) => {
                            log::error!("ws: serde error: {e}");
                            continue;
                        }
                    };
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                None => break, // every forwarder dropped
            },
            // Log tap branch — only fires when LOG_TAP was initialised
            // (i.e. running under the binary, not in a unit test).
            log = async {
                match log_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match log {
                Ok(line) => {
                    let event = SiteEvent::Log {
                        ts_ms: line.ts_ms,
                        level: line.level,
                        target: line.target,
                        message: line.message,
                    };
                    let wire = WireEvent { mg_id: None, event: &event };
                    if let Ok(json) = serde_json::to_string(&wire)
                        && socket.send(Message::Text(json.into())).await.is_err()
                    {
                        break;
                    }
                }
                Err(BroadcastRecv::Lagged(_)) => continue,
                Err(BroadcastRecv::Closed) => log_rx = None,
            },
            // Dispatch store changes — re-emit as a DispatchChanged
            // wire event tagged with the affected microgrid so the
            // SPA's per-microgrid Dispatches view refetches.
            dispatch = async {
                match dispatch_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match dispatch {
                Ok(dev) => {
                    let change = match dev.change {
                        crate::sim::dispatch::DispatchChange::Created => "created",
                        crate::sim::dispatch::DispatchChange::Updated => "updated",
                        crate::sim::dispatch::DispatchChange::Deleted => "deleted",
                    };
                    let event = SiteEvent::DispatchChanged {
                        dispatch_id: dev.dispatch_id(),
                        change,
                    };
                    let wire = WireEvent {
                        mg_id: Some(dev.microgrid_id),
                        event: &event,
                    };
                    if let Ok(json) = serde_json::to_string(&wire)
                        && socket.send(Message::Text(json.into())).await.is_err()
                    {
                        break;
                    }
                }
                Err(BroadcastRecv::Lagged(_)) => continue,
                Err(BroadcastRecv::Closed) => dispatch_rx = None,
            },
            msg = socket.recv() => match msg {
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            // A new microgrid landed in the registry (from
            // `(make-microgrid)` or POST /api/microgrids). Spawn
            // a forwarder for its site so this WS session starts
            // receiving its sample / topology_changed events.
            // Subscribers can lag if registrations burst past the
            // channel capacity — continue past Lagged because the
            // SPA can recover via reconnect, and Closed never
            // fires since Config keeps the Sender alive for the
            // process lifetime.
            new_id = async {
                match registered_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match new_id {
                Ok(id) => {
                    if subscribed_ids.contains(&id) {
                        // Race-tolerant: a Lagged-driven re-snapshot
                        // already picked this id up. Drop the dupe.
                        continue;
                    }
                    let entry = config.microgrids().lock().get(&id).cloned();
                    if let Some(e) = entry {
                        forwarders.push(spawn_forwarder(
                            id,
                            e.site.subscribe_events(),
                            fwd_tx_keepalive.clone(),
                        ));
                        subscribed_ids.insert(id);
                    } else {
                        log::warn!("ws: microgrid_registered({id}) but registry has no entry");
                    }
                }
                Err(BroadcastRecv::Lagged(n)) => {
                    // A create-burst overflowed the broadcast channel;
                    // the per-id notifications between our last
                    // recv and now are lost. Re-snapshot the registry
                    // and spawn forwarders for any entry we don't
                    // already have a subscription for.
                    log::warn!(
                        "ws: microgrid_registered channel lagged {n} ids; re-snapshotting registry",
                    );
                    let entries: Vec<(u64, crate::sim::microgrids::MicrogridEntry)> = config
                        .microgrids()
                        .lock()
                        .iter()
                        .map(|(id, e)| (*id, e.clone()))
                        .collect();
                    for (id, entry) in entries {
                        if subscribed_ids.insert(id) {
                            forwarders.push(spawn_forwarder(
                                id,
                                entry.site.subscribe_events(),
                                fwd_tx_keepalive.clone(),
                            ));
                        }
                    }
                }
                Err(BroadcastRecv::Closed) => {
                    // Config dropped its Sender; nothing more will
                    // arrive. Park this arm via the None branch of
                    // its future (awaiting pending() HERE would park
                    // the whole select loop, not just this arm) —
                    // the existing forwarders keep working.
                    registered_rx = None;
                }
            },
        }
    }
    // Socket closed (every `break` above lands here): abort the
    // forwarders now instead of letting each park until its site's
    // next event reveals the dropped mpsc.
    for h in forwarders {
        h.abort();
    }
}
