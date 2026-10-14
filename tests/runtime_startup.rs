//! Startup must fail before creating persistent state if the API cannot bind.
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> anyhow::Result<Self> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "macbot_startup_{}_{unique}_{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_failed_before_state(port: &str, expected_error: &str) -> anyhow::Result<()> {
    let scratch = Scratch::new()?;
    let config_path = scratch.0.join("config.json");
    std::fs::write(&config_path, include_str!("../config.json"))?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_macbot"))
        .env_clear()
        .env("MACBOT_CONFIG", &config_path)
        .env("MACBOT_DB", scratch.0.join("trades.sqlite"))
        .env("MACBOT_CONFIG_SNAPSHOT_DIR", scratch.0.join("snapshots"))
        .env("MACBOT_ML_WEIGHTS_PATH", scratch.0.join("weights.json"))
        .env("MACBOT_ENSEMBLE_PATH", scratch.0.join("ensemble.json"))
        .env("MACBOT_BACKTEST_ONLY", "1")
        .env("MACBOT_PORT", port)
        .current_dir(&scratch.0)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("startup did not reject invalid/unavailable port within 30s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_error),
        "unexpected failure: {stderr}"
    );
    for name in [
        "trades.sqlite",
        "snapshots",
        "weights.json",
        "ensemble.json",
    ] {
        assert!(
            !scratch.0.join(name).exists(),
            "created {name} before binding"
        );
    }
    Ok(())
}

#[test]
fn occupied_port_fails_before_persistent_state_is_created() -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    assert_failed_before_state(
        &listener.local_addr()?.port().to_string(),
        "Address already in use",
    )
}

#[test]
fn invalid_port_fails_before_persistent_state_is_created() -> anyhow::Result<()> {
    assert_failed_before_state("0", "MACBOT_PORT must be an integer between 1 and 65535")
}

struct Service(Child);

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_service(scratch: &Scratch, port: u16, snapshots: &str) -> anyhow::Result<Service> {
    Ok(Service(
        Command::new(env!("CARGO_BIN_EXE_macbot"))
            .env_clear()
            .env("MACBOT_CONFIG", scratch.0.join("config.json"))
            .env("MACBOT_DOTENV_PATH", scratch.0.join("selected.env"))
            .env("MACBOT_DB", scratch.0.join("trades.sqlite"))
            .env("MACBOT_CONFIG_SNAPSHOT_DIR", scratch.0.join(snapshots))
            .env("MACBOT_ML_WEIGHTS_PATH", scratch.0.join("weights.json"))
            .env("MACBOT_ENSEMBLE_PATH", scratch.0.join("ensemble.json"))
            .env("MACBOT_BACKTEST_ONLY", "1")
            .env("MACBOT_PORT", port.to_string())
            .current_dir(&scratch.0)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?,
    ))
}

fn unused_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

fn health_response_ready(response: &[u8]) -> bool {
    let Ok(response) = std::str::from_utf8(response) else {
        return false;
    };
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        return false;
    };
    let Some(status_line) = headers.lines().next() else {
        return false;
    };
    let mut status = status_line.split_whitespace();
    if status.next() != Some("HTTP/1.1") || status.next() != Some("200") {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(body).is_ok_and(|value| value["status"] == "ok")
}

fn wait_ready(service: &mut Service, port: u16) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Some(status) = service.0.try_wait()? {
            let mut stderr = String::new();
            if let Some(stream) = service.0.stderr.take() {
                stream.take(64 * 1024).read_to_string(&mut stderr)?;
            }
            anyhow::bail!("service exited before readiness ({status}): {stderr}");
        }
        if let Ok(mut stream) = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(50),
        ) {
            stream.set_read_timeout(Some(Duration::from_millis(100)))?;
            stream.set_write_timeout(Some(Duration::from_millis(100)))?;
            if stream
                .write_all(
                    b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .is_ok()
            {
                let mut buffer = Vec::new();
                if stream.take(64 * 1024 + 1).read_to_end(&mut buffer).is_ok()
                    && buffer.len() <= 64 * 1024
                    && health_response_ready(&buffer)
                {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!("service did not become ready within 30s")
}

#[test]
fn health_readiness_requires_a_successful_complete_liveness_response() {
    assert!(health_response_ready(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"ok\"}"
    ));
    for response in [
        b"HTTP/1.1 500 Internal Server Error\r\n\r\n{\"status\":\"ok\"}".as_slice(),
        b"HTTP/1.1 401 Unauthorized\r\n\r\n{\"status\":\"ok\"}".as_slice(),
        b"HTTP/1.1 200 OK\r\n\r\n{\"status\":\"failed\"}".as_slice(),
        b"HTTP/1.1 200 OK\r\n\r\n{\"status\":\"ok\"".as_slice(),
        b"arbitrary bytes".as_slice(),
    ] {
        assert!(!health_response_ready(response));
    }
}

#[test]
fn readiness_failure_preserves_the_actual_busy_port_error() -> anyhow::Result<()> {
    let scratch = Scratch::new()?;
    std::fs::write(
        scratch.0.join("config.json"),
        include_str!("../config.json"),
    )?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let mut service = spawn_service(&scratch, port, "busy-port-snapshots")?;
    let error = wait_ready(&mut service, port)
        .expect_err("an occupied control port must not become ready")
        .to_string();
    assert!(error.contains("service exited before readiness"), "{error}");
    assert!(error.contains("Address already in use"), "{error}");
    for name in [
        "trades.sqlite",
        "busy-port-snapshots",
        "weights.json",
        "ensemble.json",
    ] {
        assert!(
            !scratch.0.join(name).exists(),
            "created {name} before binding"
        );
    }
    Ok(())
}

#[test]
fn different_ports_cannot_share_database_and_crash_releases_ownership() -> anyhow::Result<()> {
    let scratch = Scratch::new()?;
    std::fs::write(
        scratch.0.join("config.json"),
        include_str!("../config.json"),
    )?;
    let first_port = unused_port()?;
    let mut first = spawn_service(&scratch, first_port, "first-snapshots")?;
    wait_ready(&mut first, first_port)?;
    let second_port = unused_port()?;
    let mut second = spawn_service(&scratch, second_port, "refused-snapshots")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = second.0.try_wait()? {
            assert!(!status.success());
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "duplicate service did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut stderr = String::new();
    use std::io::Read;
    second
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)?;
    assert!(
        stderr.contains("cannot acquire database ownership"),
        "{stderr}"
    );
    assert!(!scratch.0.join("refused-snapshots").exists());
    // A hard process interruption releases the kernel lease without requiring
    // an operator to delete a stale PID/lock file. SQLite recovery still runs.
    first.0.kill()?;
    first.0.wait()?;
    let mut restarted = spawn_service(&scratch, second_port, "restart-snapshots")?;
    wait_ready(&mut restarted, second_port)?;
    assert!(scratch.0.join("trades.sqlite").exists());
    Ok(())
}

#[test]
fn selected_dotenv_path_does_not_discover_an_unrelated_file() -> anyhow::Result<()> {
    let scratch = Scratch::new()?;
    std::fs::write(
        scratch.0.join("config.json"),
        include_str!("../config.json"),
    )?;
    std::fs::write(scratch.0.join(".env"), "MACBOT_DEPLOYMENT_PROFILE=live\n")?;
    let port = unused_port()?;
    let mut service = spawn_service(&scratch, port, "selected-env-snapshots")?;
    wait_ready(&mut service, port)?;
    Ok(())
}
