#![cfg(unix)]

use std::{
    io::{Read, Write},
    os::unix::process::CommandExt,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[path = "smoke/http.rs"]
mod http;
#[path = "smoke/package.rs"]
mod package;
#[path = "smoke/post.rs"]
mod post;
#[path = "smoke/pty.rs"]
mod pty;
#[path = "smoke/tui.rs"]
mod tui;

const BINARY: &str = env!("CARGO_BIN_EXE_dz");

fn run(command: &mut Command) -> Output {
    run_with_input(command, None)
}

fn run_with_input(command: &mut Command, input: Option<&str>) -> Output {
    run_with_timeout(command, input, Duration::from_secs(30))
}

fn run_with_timeout(command: &mut Command, input: Option<&str>, timeout: Duration) -> Output {
    let mut child = command
        .process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    if let Some(input) = input
        && let Err(error) = child.stdin.take().unwrap().write_all(input.as_bytes())
    {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            // Kill descendants too: npm/tar may still hold the output pipes open.
            // SAFETY: the child was started in its own process group.
            // ESRCH is harmless if the child exited just as the deadline expired.
            let _ = unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            child.wait().unwrap();
            out.join().unwrap();
            panic!(
                "{command:?} timed out; stderr: {}",
                String::from_utf8_lossy(&err.join().unwrap())
            );
        }
        thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    }
}

#[test]
fn timeout_terminates_descendants_holding_output_pipes() {
    let started = Instant::now();
    let result = std::panic::catch_unwind(|| {
        run_with_timeout(
            Command::new("sh").args(["-c", "sleep 30 & wait"]),
            None,
            Duration::from_millis(100),
        );
    });
    let error = result.unwrap_err();
    assert!(
        error
            .downcast_ref::<String>()
            .unwrap()
            .contains("timed out")
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

fn checked(command: &mut Command) -> String {
    let output = run(command);
    assert!(output.status.success(), "{command:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}
