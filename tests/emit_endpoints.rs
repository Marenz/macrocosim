//! The endpoints file `--emit-endpoints` writes once everything is bound.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn each_microgrid_lists_its_grpc_addr() {
    let dir = tempfile::TempDir::with_prefix("mc-endpoints-").unwrap();
    let mg = dir.path().join("mg.lisp");
    std::fs::write(
        &mg,
        "(make-microgrid :id 51 :name \"north\" :topology (lambda () nil))",
    )
    .unwrap();
    let endpoints = dir.path().join("endpoints.json");
    let _child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_macrocosim"))
            .args(["--ephemeral-ports", "--state-dir"])
            .arg(dir.path())
            .arg(format!("--emit-endpoints={}", endpoints.display()))
            .arg(&mg)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !endpoints.exists() {
        assert!(Instant::now() < deadline, "no endpoints file after 20 s");
        std::thread::sleep(Duration::from_millis(50));
    }
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&endpoints).unwrap()).unwrap();
    let mg0 = &v["microgrids"][0];
    assert_eq!(mg0["id"], 51, "{v}");
    assert_eq!(mg0["name"], "north", "{v}");
    let addr = mg0["grpc_addr"].as_str().expect("grpc_addr is a string");
    assert!(addr.contains(':'), "{addr}");
    assert!(mg0.get("grpc").is_none(), "{v}");
}
