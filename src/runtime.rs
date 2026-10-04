//! One startup path per microgrid: physics tick, history sampler,
//! Microgrid gRPC server and UI loopback client. A microgrid that
//! cannot bind, or whose server ends, is marked failed; the process
//! stays up. Nothing is ever torn down, and a microgrid's gRPC
//! address is fixed after its first successful bind.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

use crate::lisp::Config;
use crate::proto::microgrid::microgrid_server::MicrogridServer as MicrogridGrpcServer;
use crate::server::MicrogridServer;
use crate::sim::MicrogridSite;
use crate::ui::{self, MicrogridLoopbacks};

/// How [`MicrogridRuntimes::start`] binds a microgrid's gRPC server.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeOptions {
    /// Bind port 0 (OS-chosen) on the first bind.
    pub ephemeral_ports: bool,
    pub bind_host: IpAddr,
}

/// Where a runtime is, as [`RuntimeView`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeStatus {
    Starting,
    Running,
    Failed,
}

/// A runtime as `/api/microgrids` and the create routes report it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RuntimeView {
    pub status: RuntimeStatus,
    pub grpc_addr: Option<String>,
    pub error: Option<String>,
}

/// Why [`MicrogridRuntimes::start`] could not start a microgrid.
#[derive(Debug)]
pub enum StartError {
    NotRegistered(u64),
    SiteReplaced(u64),
    Bind { addr: SocketAddr, error: String },
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRegistered(id) => write!(f, "microgrid {id} is not registered"),
            Self::SiteReplaced(id) => {
                write!(f, "microgrid {id}: site replaced; restart macrocosim")
            }
            Self::Bind { addr, error } => write!(f, "gRPC bind {addr} failed: {error}"),
        }
    }
}

impl std::error::Error for StartError {}

#[derive(Clone, Debug)]
enum Status {
    Starting,
    Running,
    Failed(String),
}

struct Runtime {
    status: Status,
    /// Set by the first successful bind; every later attempt
    /// rebinds it.
    grpc_addr: Option<SocketAddr>,
    site: MicrogridSite,
    /// Bumped on every attempt; a server watcher only touches the
    /// status of its own attempt.
    generation: u64,
}

struct Inner {
    config: Config,
    options: RuntimeOptions,
    runtimes: Mutex<BTreeMap<u64, Runtime>>,
    /// Serializes `start`, so two callers cannot start one microgrid
    /// twice.
    start_lock: tokio::sync::Mutex<()>,
}

/// The per-microgrid runtimes, keyed by microgrid id.
#[derive(Clone)]
pub struct MicrogridRuntimes {
    /// `None` for [`Self::inert`].
    inner: Option<Arc<Inner>>,
    loopbacks: MicrogridLoopbacks,
}

impl MicrogridRuntimes {
    /// Refuses a headless `Config`: wall-clock physics must never
    /// start on one. Installs the port-pin check `make-microgrid`
    /// asks on a reload, and refuses a `Config` that already has one:
    /// one `Config` has one set of runtimes.
    pub fn new(config: Config, options: RuntimeOptions) -> Result<Self, String> {
        if config.is_headless() {
            return Err("MicrogridRuntimes cannot run a headless Config".into());
        }
        let inner = Arc::new(Inner {
            config: config.clone(),
            options,
            runtimes: Mutex::new(BTreeMap::new()),
            start_lock: tokio::sync::Mutex::new(()),
        });
        let weak = Arc::downgrade(&inner);
        config
            .port_pins()
            .set(Arc::new(move |id| {
                weak.upgrade().is_some_and(|inner| {
                    inner.runtimes.lock().get(&id).is_some_and(|rt| {
                        rt.grpc_addr.is_some() || matches!(rt.status, Status::Starting)
                    })
                })
            }))
            .map_err(|_| "this Config already has a MicrogridRuntimes")?;
        Ok(Self {
            inner: Some(inner),
            loopbacks: ui::new_microgrid_loopbacks(),
        })
    }

    /// Starts nothing: `start` answers `Ok(None)`, `status` `None`.
    pub fn inert() -> Self {
        Self {
            inner: None,
            loopbacks: ui::new_microgrid_loopbacks(),
        }
    }

    /// The loopback slots the UI routes read.
    pub fn loopbacks(&self) -> MicrogridLoopbacks {
        self.loopbacks.clone()
    }

    pub fn status(&self, id: u64) -> Option<RuntimeView> {
        let inner = self.inner.as_ref()?;
        let rts = inner.runtimes.lock();
        let rt = rts.get(&id)?;
        let (status, error) = match &rt.status {
            Status::Starting => (RuntimeStatus::Starting, None),
            Status::Running => (RuntimeStatus::Running, None),
            Status::Failed(e) => (RuntimeStatus::Failed, Some(e.clone())),
        };
        Some(RuntimeView {
            status,
            grpc_addr: rt.grpc_addr.map(|a| a.to_string()),
            error,
        })
    }

    /// Start `id`'s runtime, or return the address of the one already
    /// running. Safe to call any number of times.
    pub async fn start(&self, id: u64) -> Result<Option<SocketAddr>, StartError> {
        let Some(inner) = &self.inner else {
            return Ok(None);
        };
        let _serialized = inner.start_lock.lock().await;
        // Registry, then status: the order make-microgrid's pin check
        // takes them in. The port is read under both, so a reload
        // cannot move it once this runtime is Starting.
        let (site, addr, generation) = {
            let reg = inner.config.microgrids();
            let reg = reg.lock();
            let e = reg.get(&id).ok_or(StartError::NotRegistered(id))?;
            let site = e.site.clone();
            let mut rts = inner.runtimes.lock();
            let (fixed, generation) = match rts.get_mut(&id) {
                Some(rt) if !rt.site.ptr_eq(&site) => {
                    let err = StartError::SiteReplaced(id);
                    // A new generation, so the old server's watcher
                    // cannot overwrite this error.
                    rt.generation += 1;
                    rt.status = Status::Failed(err.to_string());
                    return Err(err);
                }
                Some(rt) if matches!(rt.status, Status::Running) => return Ok(rt.grpc_addr),
                Some(rt) => {
                    rt.status = Status::Starting;
                    rt.generation += 1;
                    (rt.grpc_addr, rt.generation)
                }
                None => {
                    rts.insert(
                        id,
                        Runtime {
                            status: Status::Starting,
                            grpc_addr: None,
                            site: site.clone(),
                            generation: 1,
                        },
                    );
                    (None, 1)
                }
            };
            let addr = match fixed {
                Some(addr) => addr,
                None if inner.options.ephemeral_ports => {
                    SocketAddr::new(inner.options.bind_host, 0)
                }
                None => SocketAddr::new(inner.options.bind_host, e.def.grpc_port),
            };
            (site, addr, generation)
        };
        let slot = self
            .loopbacks
            .write()
            .entry(id)
            .or_insert_with(ui::new_microgrid_slot)
            .clone();
        site.spawn_background();
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                let err = StartError::Bind {
                    addr,
                    error: e.to_string(),
                };
                set_status(inner, id, generation, Status::Failed(err.to_string()));
                return Err(err);
            }
        };
        let bound = listener.local_addr().unwrap_or(addr);
        // Marked running before the server task exists, so a server
        // that ends at once leaves the microgrid failed.
        let first_bind = {
            let mut rts = inner.runtimes.lock();
            let rt = rts.get_mut(&id).expect("runtime inserted above");
            let first = rt.grpc_addr.replace(bound).is_none();
            if rt.generation == generation {
                rt.status = Status::Running;
            }
            first
        };
        let server = MicrogridServer::new(inner.config.clone(), id, site.clone());
        let handle = tokio::spawn(
            Server::builder()
                .add_service(MicrogridGrpcServer::new(server))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        tokio::spawn(watch_server(Arc::downgrade(inner), id, generation, handle));
        if first_bind {
            ui::spawn_microgrid_loopback(format!("http://{bound}"), slot, site);
        }
        log::info!("Microgrid #{id} gRPC listening on {bound}");
        Ok(Some(bound))
    }
}

fn set_status(inner: &Inner, id: u64, generation: u64, status: Status) {
    if let Some(rt) = inner.runtimes.lock().get_mut(&id)
        && rt.generation == generation
    {
        rt.status = status;
    }
}

/// Await a gRPC server task and mark its microgrid failed when it
/// ends, whether it returned or panicked.
async fn watch_server(
    inner: Weak<Inner>,
    id: u64,
    generation: u64,
    handle: JoinHandle<Result<(), tonic::transport::Error>>,
) {
    let reason = match handle.await {
        Ok(Ok(())) => "gRPC server exited".to_string(),
        Ok(Err(e)) => format!("gRPC server error: {e}"),
        Err(e) if e.is_panic() => {
            let p = e.into_panic();
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            format!("gRPC server panicked: {msg}")
        }
        Err(e) => format!("gRPC server task ended: {e}"),
    };
    log::error!("Microgrid #{id}: {reason}");
    if let Some(inner) = inner.upgrade() {
        set_status(&inner, id, generation, Status::Failed(reason));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv6Addr, SocketAddr};

    const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

    /// A port nobody holds right now, for tests that need a fixed
    /// one.
    fn free_v6_port() -> u16 {
        std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A temp dir holding one file that declares microgrid `id`.
    fn mg_file(id: u64, port: u16) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::with_prefix("mc-runtime-").unwrap();
        let path = dir.path().join("mg.lisp");
        std::fs::write(&path, mg_decl(id, port)).unwrap();
        (dir, path)
    }

    fn config_with_mg(id: u64, port: u16) -> (Config, tempfile::TempDir) {
        let (dir, path) = mg_file(id, port);
        (Config::new(path.to_str().unwrap()).unwrap(), dir)
    }

    /// Hand `watch_server` an already-ended server task for `id`.
    async fn end_server(inner: &Arc<Inner>, id: u64, generation: u64) {
        let ended = tokio::spawn(async { Ok::<(), tonic::transport::Error>(()) });
        watch_server(Arc::downgrade(inner), id, generation, ended).await;
    }

    fn mg_decl(id: u64, port: u16) -> String {
        format!("(make-microgrid :id {id} :grpc-port {port} :topology (lambda () nil))")
    }

    fn opts(ephemeral: bool) -> RuntimeOptions {
        RuntimeOptions {
            ephemeral_ports: ephemeral,
            bind_host: V6,
        }
    }

    #[tokio::test]
    async fn start_twice_returns_the_same_address() {
        let (cfg, _d) = config_with_mg(41, free_v6_port());
        let rt = MicrogridRuntimes::new(cfg, opts(true)).unwrap();
        let a = rt.start(41).await.unwrap().unwrap();
        let b = rt.start(41).await.unwrap().unwrap();
        assert_eq!(a, b);
        assert_eq!(rt.status(41).unwrap().status, RuntimeStatus::Running);
        assert_eq!(rt.status(41).unwrap().grpc_addr, Some(a.to_string()));
    }

    #[tokio::test]
    async fn a_held_port_fails_and_a_freed_one_retries() {
        let port = free_v6_port();
        let holder = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port)).unwrap();
        let (cfg, _d) = config_with_mg(42, port);
        let rt = MicrogridRuntimes::new(cfg, opts(false)).unwrap();
        assert!(matches!(rt.start(42).await, Err(StartError::Bind { .. })));
        let v = rt.status(42).unwrap();
        assert_eq!(v.status, RuntimeStatus::Failed);
        assert!(v.error.unwrap().contains(&port.to_string()));
        drop(holder);
        let addr = rt.start(42).await.unwrap().unwrap();
        assert_eq!(addr, SocketAddr::new(V6, port));
    }

    #[tokio::test]
    async fn an_unbound_microgrid_adopts_a_new_declared_port() {
        let port = free_v6_port();
        let holder = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port)).unwrap();
        let (_dir, file) = mg_file(43, port);
        let cfg = Config::new(file.to_str().unwrap()).unwrap();
        let rt = MicrogridRuntimes::new(cfg.clone(), opts(false)).unwrap();
        assert!(matches!(rt.start(43).await, Err(StartError::Bind { .. })));
        assert!(
            rt.loopbacks()
                .read()
                .get(&43)
                .unwrap()
                .client
                .get()
                .is_none()
        );
        let new_port = free_v6_port();
        std::fs::write(&file, mg_decl(43, new_port)).unwrap();
        cfg.load_file(&file).unwrap();
        let addr = rt.start(43).await.unwrap().unwrap();
        assert_eq!(addr.port(), new_port);
        drop(holder);
    }

    #[tokio::test]
    async fn a_bound_microgrid_keeps_its_port_on_reload() {
        let port = free_v6_port();
        let (_dir, file) = mg_file(44, port);
        let cfg = Config::new(file.to_str().unwrap()).unwrap();
        let rt = MicrogridRuntimes::new(cfg.clone(), opts(false)).unwrap();
        rt.start(44).await.unwrap();
        std::fs::write(&file, mg_decl(44, free_v6_port())).unwrap();
        cfg.load_file(&file).unwrap();
        assert_eq!(cfg.microgrids().lock()[&44].def.grpc_port, port);
    }

    #[tokio::test]
    async fn physics_starts_once_across_starts() {
        let (cfg, _d) = config_with_mg(45, free_v6_port());
        let site = cfg.microgrids().lock()[&45].site.clone();
        let rt = MicrogridRuntimes::new(cfg, opts(true)).unwrap();
        rt.start(45).await.unwrap();
        rt.start(45).await.unwrap();
        assert!(!site.spawn_background(), "start left physics unstarted");
    }

    #[tokio::test]
    async fn unknown_id_errors_and_records_nothing() {
        let (cfg, _d) = config_with_mg(46, free_v6_port());
        let rt = MicrogridRuntimes::new(cfg, opts(true)).unwrap();
        assert!(matches!(
            rt.start(999).await,
            Err(StartError::NotRegistered(999))
        ));
        assert!(rt.status(999).is_none());
    }

    #[tokio::test]
    async fn a_headless_config_is_refused() {
        let (_dir, path) = mg_file(54, free_v6_port());
        let (cfg, _clock) = Config::new_headless(path.to_str().unwrap()).unwrap();
        assert!(MicrogridRuntimes::new(cfg, opts(true)).is_err());
    }

    /// One Config holds one port-pin check, so a second runtimes
    /// instance on it is refused and the first keeps answering.
    #[tokio::test]
    async fn a_second_runtimes_on_one_config_is_refused() {
        let (cfg, _d) = config_with_mg(55, free_v6_port());
        let first = MicrogridRuntimes::new(cfg.clone(), opts(true)).unwrap();
        let second = MicrogridRuntimes::new(cfg.clone(), opts(true));
        assert!(second.is_err_and(|e| e.contains("already")));
        first.start(55).await.unwrap();
        assert!(crate::sim::microgrids::port_pinned(&cfg.port_pins(), 55));
    }

    /// A runtime that is still starting holds its port, so a reload
    /// cannot move it under the bind.
    #[tokio::test]
    async fn a_starting_runtime_holds_its_port() {
        let (cfg, _d) = config_with_mg(56, free_v6_port());
        let site = cfg.microgrids().lock()[&56].site.clone();
        let rt = MicrogridRuntimes::new(cfg.clone(), opts(true)).unwrap();
        rt.inner.as_ref().unwrap().runtimes.lock().insert(
            56,
            Runtime {
                status: Status::Starting,
                grpc_addr: None,
                site,
                generation: 1,
            },
        );
        assert!(crate::sim::microgrids::port_pinned(&cfg.port_pins(), 56));
    }

    #[tokio::test]
    async fn inert_starts_nothing() {
        let rt = MicrogridRuntimes::inert();
        assert_eq!(rt.start(1).await.unwrap(), None);
        assert!(rt.status(1).is_none());
    }

    /// The address is fixed after the first bind: under ephemeral
    /// ports a recomputed one would be a fresh OS-chosen port. The
    /// first server still holds the port, so the restart's bind of
    /// that same address fails.
    #[tokio::test]
    async fn a_failed_microgrid_restarts_on_its_first_address() {
        let (cfg, _d) = config_with_mg(52, free_v6_port());
        let rt = MicrogridRuntimes::new(cfg, opts(true)).unwrap();
        let a = rt.start(52).await.unwrap().unwrap();
        let inner = rt.inner.clone().unwrap();
        let generation = inner.runtimes.lock()[&52].generation;
        end_server(&inner, 52, generation).await;
        assert_eq!(rt.status(52).unwrap().status, RuntimeStatus::Failed);
        match rt.start(52).await {
            Err(StartError::Bind { addr, .. }) => assert_eq!(addr, a),
            other => panic!("expected a bind of {a}, got {other:?}"),
        }
        assert_eq!(rt.status(52).unwrap().grpc_addr, Some(a.to_string()));
    }

    /// The old server's watcher cannot overwrite the site-replaced
    /// error.
    #[tokio::test]
    async fn a_replaced_site_stays_failed_when_the_old_server_ends() {
        let (cfg, _d) = config_with_mg(53, free_v6_port());
        let rt = MicrogridRuntimes::new(cfg, opts(true)).unwrap();
        rt.start(53).await.unwrap();
        let inner = rt.inner.clone().unwrap();
        let generation = {
            let mut rts = inner.runtimes.lock();
            let r = rts.get_mut(&53).unwrap();
            r.site = MicrogridSite::new();
            r.generation
        };
        assert!(matches!(
            rt.start(53).await,
            Err(StartError::SiteReplaced(53))
        ));
        end_server(&inner, 53, generation).await;
        assert!(
            rt.status(53)
                .unwrap()
                .error
                .unwrap()
                .contains("site replaced")
        );
    }

    #[tokio::test]
    async fn a_server_that_returns_or_panics_marks_the_microgrid_failed() {
        let (cfg, _d) = config_with_mg(47, free_v6_port());
        let rt = MicrogridRuntimes::new(cfg, opts(true)).unwrap();
        rt.start(47).await.unwrap();
        let inner = rt.inner.clone().unwrap();
        let generation = inner.runtimes.lock()[&47].generation;
        end_server(&inner, 47, generation).await;
        assert_eq!(rt.status(47).unwrap().status, RuntimeStatus::Failed);
        let panicked = tokio::spawn(async { panic!("boom") });
        watch_server(std::sync::Arc::downgrade(&inner), 47, generation, panicked).await;
        assert!(rt.status(47).unwrap().error.unwrap().contains("boom"));
    }
}
