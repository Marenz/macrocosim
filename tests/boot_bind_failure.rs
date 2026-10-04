//! The binary exits when a boot microgrid's gRPC port is taken.

use std::net::{Ipv6Addr, TcpListener};
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn a_boot_microgrid_on_a_held_port_exits_the_binary() {
    let holder = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
    let port = holder.local_addr().unwrap().port();
    let dir = tempfile::TempDir::with_prefix("mc-bootfail-").unwrap();
    std::fs::write(
        dir.path().join("enterprise.lisp"),
        "(set-assets-socket-addr \"[::1]:0\")\n(set-dispatch-socket-addr \"[::1]:0\")\n",
    )
    .unwrap();
    let mg = dir.path().join("mg.lisp");
    std::fs::write(
        &mg,
        format!("(make-microgrid :id 51 :grpc-port {port} :topology (lambda () nil))"),
    )
    .unwrap();
    let log_path = dir.path().join("out.log");
    let out = std::fs::File::create(&log_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_macrocosim"))
        .args(["--ui-port", "0", "--state-dir"])
        .arg(dir.path())
        .arg(&mg)
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap();
    // A binary that stops exiting would serve forever: fail instead
    // of hanging the test run.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("the binary did not exit:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success(), "the binary must exit with an error");
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("Microgrid #51") && log.contains(&port.to_string()),
        "{log}"
    );
    drop(holder);
}
