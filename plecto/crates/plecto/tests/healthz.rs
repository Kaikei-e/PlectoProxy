//! E2E (tdd-workflow Phase 0) for `plecto healthz` — the self-probe subcommand that lets a
//! shell-less (distroless) container image drive a Docker/Compose `healthcheck:` without curl
//! or wget (field report §3.6). Drives the real compiled binary (`CARGO_BIN_EXE_plecto`).
//!
//! Contract under test:
//! - `plecto healthz <manifest.toml>` probes `GET /readyz` on the manifest's
//!   `[observability] admin_addr` (readiness is what a Compose `service_healthy` gate means).
//! - `--live` probes `GET /healthz` (liveness) instead.
//! - `--admin-addr <host:port>` overrides / replaces the manifest lookup.
//! - Exit code is 0 on a 2xx response and 1 otherwise — never 2, which the Dockerfile
//!   HEALTHCHECK contract reserves.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn run(args: &[&str], dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_plecto"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

/// Reserve an ephemeral port by binding and immediately dropping a listener (same approach as
/// `binary_signals.rs`): slightly racy, but the binary takes explicit addresses so this is the
/// portable way to point the probe at it without hardcoding ports.
fn free_port_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn manifest_with_admin(listen: SocketAddr, admin: SocketAddr) -> String {
    format!(
        r#"[listen]
addr = "{listen}"

[observability]
admin_addr = "{admin}"
"#
    )
}

/// Kills the serving child on drop so a failing assertion never leaks a listener process.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_serving(dir: &Path, manifest: &str) -> KillOnDrop {
    std::fs::write(dir.join("plecto.toml"), manifest).unwrap();
    KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_plecto"))
            .arg("plecto.toml")
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

/// Retry the probe until it succeeds or `deadline` passes — the probe itself is the readiness
/// wait, which is exactly how a Compose healthcheck uses it during the `starting` window.
fn probe_until_healthy(args: &[&str], dir: &Path, deadline: Duration) -> std::process::Output {
    let start = Instant::now();
    loop {
        let out = run(args, dir);
        if out.status.success() || start.elapsed() > deadline {
            return out;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn healthz_exits_zero_against_a_serving_admin_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (listen, admin) = (free_port_addr(), free_port_addr());
    let _child = spawn_serving(dir.path(), &manifest_with_admin(listen, admin));

    let ready = probe_until_healthy(
        &["healthz", "plecto.toml"],
        dir.path(),
        Duration::from_secs(10),
    );
    assert!(
        ready.status.success(),
        "healthz exits 0 once /readyz answers 200, got {:?} stderr: {}",
        ready.status.code(),
        String::from_utf8_lossy(&ready.stderr)
    );
    // Probe output is captured by `docker inspect` health logs (first 4096 bytes) — keep it to
    // one short line.
    let stdout = String::from_utf8_lossy(&ready.stdout);
    assert_eq!(
        stdout.trim().lines().count(),
        1,
        "one-line output: {stdout:?}"
    );

    let live = run(&["healthz", "--live", "plecto.toml"], dir.path());
    assert!(
        live.status.success(),
        "--live probes /healthz, got {:?} stderr: {}",
        live.status.code(),
        String::from_utf8_lossy(&live.stderr)
    );
}

#[test]
fn healthz_accepts_an_explicit_admin_addr_without_a_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let (listen, admin) = (free_port_addr(), free_port_addr());
    let _child = spawn_serving(dir.path(), &manifest_with_admin(listen, admin));

    let out = probe_until_healthy(
        &["healthz", "--admin-addr", &admin.to_string()],
        dir.path(),
        Duration::from_secs(10),
    );
    assert!(
        out.status.success(),
        "--admin-addr overrides the manifest lookup, got {:?} stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn healthz_exits_one_when_nothing_listens() {
    let dir = tempfile::tempdir().unwrap();
    // A reserved-then-released port: valid address, nothing listening.
    let admin = free_port_addr();
    let out = run(&["healthz", "--admin-addr", &admin.to_string()], dir.path());
    assert_eq!(
        out.status.code(),
        Some(1),
        "connection failure is unhealthy (exit 1, never the Docker-reserved 2), stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn healthz_exits_one_when_the_manifest_has_no_admin_addr() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("plecto.toml"), "").unwrap();
    let out = run(&["healthz", "plecto.toml"], dir.path());
    assert_eq!(
        out.status.code(),
        Some(1),
        "no admin endpoint to probe = exit 1"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("admin_addr"),
        "the error names the missing [observability] admin_addr knob, got: {stderr}"
    );
}

#[test]
fn healthz_usage_error_exits_one_not_two() {
    // The Dockerfile HEALTHCHECK contract reserves exit code 2 — even a bad invocation from a
    // mistyped Compose `test:` line must not produce it.
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["healthz", "--admin-addr"], dir.path());
    assert_eq!(out.status.code(), Some(1), "usage error exits 1, not 2");
}
