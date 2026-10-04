//! Integration-test harness: spawn a `Config`-driven macrocosim
//! server in-process on OS-assigned ports, expose its gRPC + UI
//! addresses, and tear everything down on Drop.
//!
//! Each test gets its own temp dir for `config.lisp` and a fresh
//! `Config`, so parallel tests can't stomp each other's state.
//!
//! The fixture is in-process rather than out-of-process because:
//! - cargo runs each `tests/<file>.rs` as its own binary already,
//!   so OS-level isolation is overkill.
//! - In-process tests can poke at `cfg.site()` directly when the
//!   black-box gRPC / HTTP surface isn't enough.
//! - LOG_TAP and other process-level globals stay un-initialised in
//!   tests, so the `/api/logs` endpoint just returns empty —
//!   acceptable for a fixture.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use macrocosim::{
    assets_server::AssetsServer, lisp::Config,
    proto::assets::platform_assets_server::PlatformAssetsServer as AssetsGrpcServer,
    proto::microgrid::microgrid_server::MicrogridServer as MicrogridGrpcServer,
    server::MicrogridServer, sim::MicrogridSite, ui,
};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// A live macrocosim instance: gRPC + UI on OS-assigned localhost
/// ports, plus the underlying [`Config`] for direct world
/// inspection. `Drop` aborts the spawned server tasks; the temp
/// dir cleans up via the held `TempDir` handle.
///
/// Each integration-test binary picks the fields it needs; the
/// `#[allow(dead_code)]` keeps the unused-warning quiet for tests
/// that only touch one surface.
#[allow(dead_code)]
pub struct TestServer {
    pub grpc_url: String,
    pub ui_url: String,
    pub config: Config,
    handles: Vec<JoinHandle<()>>,
    _tempdir: TempDir,
}

impl TestServer {
    /// Bring up a server backed by the supplied `config.lisp` body.
    /// Caller is on a tokio runtime (provided by `#[tokio::test]`).
    pub async fn start(config_body: &str) -> Self {
        let tempdir = TempDir::with_prefix(format!(
            "macrocosim-it-{}-",
            UNIQ.fetch_add(1, Ordering::Relaxed),
        ))
        .expect("create temp dir");
        let path = tempdir.path().join("config.lisp");
        let wrapped = wrap_body(config_body);
        std::fs::write(&path, wrapped).expect("write config");

        let config = Config::new(path.to_str().unwrap()).expect("config eval");
        // Physics + history sampler match the prod boot sequence.
        MicrogridSite::clone(&config.site()).spawn_physics();
        MicrogridSite::clone(&config.site()).spawn_history_sampler();

        // Bind both servers to OS-assigned ports so parallel tests
        // don't collide. local_addr() reads back the chosen port
        // before we hand the listener off to the server.
        let ui_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ui port");
        let ui_addr = ui_listener.local_addr().expect("ui addr");

        let grpc_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind grpc port");
        let grpc_addr = grpc_listener.local_addr().expect("grpc addr");

        let mut handles = Vec::new();

        let ui_config = config.clone();
        // Loopback Microgrid client: same shape the binary uses.
        // The grpc_addr we just bound is
        // the URL — try_new retries lazily until the gRPC server
        // task below comes up. Integration tests for the
        // /api/microgrid/* endpoints exercise this whole loop.
        let microgrid = ui::new_microgrid_slot();
        ui::spawn_microgrid_loopback(
            format!("http://{grpc_addr}"),
            microgrid.clone(),
            config.site(),
        );
        // Single-microgrid integration test: pin the gRPC frontend
        // to the default registry entry (the one auto-seeded by
        // Config::new when no `(make-microgrid)` form ran, or the
        // id an explicit form in `config_body` chose) — matches
        // what `get_microgrid` reports.
        let default_mg_id = {
            let reg = config.microgrids();
            let r = reg.lock();
            r.keys().copied().next().expect("default microgrid entry")
        };

        // Single-microgrid integration test: inert runtimes whose
        // loopbacks map holds one entry, keyed by the real microgrid
        // id so the per-mg /api/mg/{id}/microgrid/* routes resolve.
        // /api/microgrid/* keeps reading the primary slot.
        let runtimes = macrocosim::runtime::MicrogridRuntimes::inert();
        runtimes
            .loopbacks()
            .write()
            .insert(default_mg_id, microgrid.clone());
        handles.push(tokio::spawn(async move {
            let _ = ui::serve_with_listener(ui_listener, ui_config, microgrid, runtimes).await;
        }));
        let microgrid_server = MicrogridServer::new(config.clone(), default_mg_id, config.site());
        let assets_server = AssetsServer::new(config.clone());
        handles.push(tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(MicrogridGrpcServer::new(microgrid_server))
                .add_service(AssetsGrpcServer::new(assets_server))
                .serve_with_incoming(TcpListenerStream::new(grpc_listener))
                .await;
        }));

        // Wait until the UI server actually answers a request
        // instead of sleeping a fixed 50 ms — on a loaded machine
        // the accept loops can take longer than any fixed delay,
        // and this poll returns as soon as they are up. The gRPC
        // listener needs no probe of its own: its socket is bound
        // above, so connects queue until tonic starts accepting.
        let probe = reqwest::Client::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match probe
                .get(format!("http://{ui_addr}/api/microgrids"))
                .send()
                .await
            {
                Ok(_) => break,
                Err(e) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "UI server at {ui_addr} not ready after 10s: {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }

        Self {
            grpc_url: format!("http://{grpc_addr}"),
            ui_url: format!("http://{ui_addr}"),
            config,
            handles,
            _tempdir: tempdir,
        }
    }

    /// Path of the config.lisp file backing this server. Tests
    /// that exercise the watcher (hot-reload) overwrite this file
    /// to trigger a reload.
    #[allow(dead_code)]
    pub fn config_path(&self) -> PathBuf {
        self._tempdir.path().join("config.lisp")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        for h in &self.handles {
            h.abort();
        }
    }
}

/// POST a Lisp body to `/api/eval` and fail the test unless the
/// server answers `{"ok": true}`. The shared spelling of "drive the
/// sim the way the dashboard's REPL does" every integration test that
/// needs a live eval uses.
#[allow(dead_code)]
pub async fn eval_or_panic(client: &reqwest::Client, s: &TestServer, body: &str) {
    let r = client
        .post(format!("{}/api/eval", s.ui_url))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status();
    let json: serde_json::Value = r.json().await.unwrap();
    assert!(
        status.is_success() && json["ok"] == true,
        "eval {body} failed: {status} {json}",
    );
}

/// Wrap a test body in `(make-microgrid …)` if the body doesn't
/// already register one. Tests that care about the microgrid's id
/// supply their own `(make-microgrid …)` form; everything else gets
/// the fixed default id 2200.
fn wrap_body(body: &str) -> String {
    if body.contains("make-microgrid") {
        return body.to_string();
    }
    let inner = if body.trim().is_empty() {
        "nil".to_string()
    } else {
        body.to_string()
    };
    format!("(make-microgrid :id 2200 :grpc-port 8800 :topology (lambda () {inner}))")
}
