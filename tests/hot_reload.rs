//! Hot-reload integration test. Edit the config file on disk and
//! assert the world rebuilds with the new content. Mirrors what
//! the production binary does when the user saves config.lisp.

mod common;

use std::time::Duration;

use common::TestServer;
use macrocosim::lisp::Config;
use serde_json::Value;

#[tokio::test(flavor = "multi_thread")]
async fn editing_config_lisp_rebuilds_the_world() {
    // Initial topology: just a grid.
    let initial = "(make-microgrid :id 9 :grpc-port 8800 :topology \
                   (lambda () (%make-grid-connection-point :id 1)))\n";
    let s = TestServer::start(initial).await;
    // Spawn the watcher — production path is `tokio::spawn(config.clone().watch())`;
    // the readiness signal sequences our rewrite after the inotify
    // hooks exist, where a sleep only made the race unlikely.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(Config::clone(&s.config).watch_with_ready(Some(ready_tx)));
    ready_rx.await.expect("watch setup resolved");

    let v0 = s.config.site().version();
    let path = s.config_path();
    // The handle the running physics task / per-port gRPC server
    // holds. Reload must rebuild the topology on THIS site, not a
    // fresh one — otherwise every runtime is orphaned.
    let live_site = s
        .config
        .microgrids()
        .lock()
        .get(&9)
        .expect("microgrid 9 registered at boot")
        .site
        .clone();
    assert!(live_site.get(1).is_some());

    // Append a meter under the grid by replacing the file. The
    // notify watcher fires on close-after-write (inotify) and the
    // reload path resets the world and re-evals.
    let updated = "(make-microgrid :id 9 :grpc-port 8800 :topology \
                   (lambda () \
                     (%make-grid-connection-point :id 1 \
                      :successors (list (%make-meter :id 2)))))\n";
    std::fs::write(&path, updated).expect("rewrite config");

    // Poll for the version to bump — bounded so the test fails
    // loudly rather than hanging if the watcher misfires.
    let mut versioned = false;
    for _ in 0..50 {
        if s.config.site().version() > v0 {
            versioned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(versioned, "world version never bumped after rewrite");

    // The new meter shows up via the topology route.
    let topo: Value = reqwest::get(s.mg_url("topology"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<i64> = topo["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&2), "expected id 2 after reload, got {ids:?}");

    // Runtime continuity: the pre-reload site handle sees the new
    // meter too — the rebuilt topology landed on the same site the
    // running physics loop and gRPC server are pinned to.
    assert!(
        live_site.get(2).is_some(),
        "pre-reload site handle must see the rebuilt topology",
    );
}
