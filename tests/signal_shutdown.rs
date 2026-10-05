#![cfg(unix)]

use std::error::Error;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

fn wait_for_socket(child: &mut Child, socket: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if socket.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "daemon exited before creating socket: {status}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon did not create its socket",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_and_expect_success(mut child: Child, socket: &Path) -> TestResult {
    wait_for_socket(&mut child, socket)?;
    // Both daemons bind before installing their shared signal handler. Give
    // the event loop a small deterministic margin after socket readiness.
    thread::sleep(Duration::from_millis(100));

    let raw_pid =
        i32::try_from(child.id()).map_err(|_| io::Error::other("child PID does not fit pid_t"))?;
    let pid = Pid::from_raw(raw_pid).ok_or_else(|| io::Error::other("invalid child PID"))?;
    kill_process(pid, Signal::TERM)?;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(io::Error::other(format!(
                "daemon did not shut down cleanly after SIGTERM: {status}"
            ))
            .into());
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _status = child.wait()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon did not exit after SIGTERM",
            )
            .into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn checkpoint_handles_sigterm_gracefully() -> TestResult {
    let temp = TempDir::new()?;
    let source = temp.path().join("source");
    let writeback = temp.path().join("writeback");
    let state = temp.path().join("state");
    let namespace = temp.path().join("namespace");
    let rename = temp.path().join("rename");
    let socket = temp.path().join("checkpoint.sock");
    for path in [&source, &writeback, &state, &namespace, &rename] {
        fs::create_dir_all(path)?;
    }
    let source_prefix = format!("{}/", source.display());

    let child = Command::new(env!("CARGO_BIN_EXE_moraine-checkpoint"))
        .arg("--root")
        .arg(&writeback)
        .arg("--state-root")
        .arg(&state)
        .arg("--namespace-root")
        .arg(&namespace)
        .arg("--rename-root")
        .arg(&rename)
        .arg("--socket")
        .arg(&socket)
        .arg("--source-prefix")
        .arg(source_prefix)
        .arg("--durability")
        .arg("file")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    terminate_and_expect_success(child, &socket)
}

#[test]
fn admission_server_handles_sigterm_gracefully() -> TestResult {
    let temp = TempDir::new()?;
    let micro_root = temp.path().join("micro");
    let socket = temp.path().join("admit.sock");
    fs::create_dir_all(&micro_root)?;

    let child = Command::new(env!("CARGO_BIN_EXE_moraine-admit"))
        .arg("--micro-root")
        .arg(&micro_root)
        .arg("--workers")
        .arg("1")
        .arg("serve")
        .arg("--socket")
        .arg(&socket)
        .arg("--cooldown")
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    terminate_and_expect_success(child, &socket)
}
