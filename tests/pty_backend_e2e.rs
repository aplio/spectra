#![cfg(unix)]

use std::time::{Duration, Instant};

use spectra::session::pane::PaneBackend;
use spectra::session::pty_backend::{PaneFactory, PaneSpawnConfig, PtyPaneFactory};

fn shell_path() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn collect_output_until(
    backend: &mut dyn PaneBackend,
    timeout: Duration,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let start = Instant::now();
    let mut output = Vec::new();

    while start.elapsed() < timeout {
        for chunk in backend.poll_output() {
            output.extend_from_slice(&chunk);
        }

        let text = String::from_utf8_lossy(&output);
        if predicate(&text) {
            return text.into_owned();
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    String::from_utf8_lossy(&output).into_owned()
}

fn wait_until_closed(backend: &mut dyn PaneBackend, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let _ = backend.poll_output();
        if backend.is_closed() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn spawn_uses_tty() {
    let factory = PtyPaneFactory;
    let mut backend = factory
        .spawn(&PaneSpawnConfig {
            shell: shell_path(),
            cwd: None,
            command: vec!["[ -t 0 ] && printf 'TTY=1\\n' || printf 'TTY=0\\n'".to_string()],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
        })
        .expect("spawn backend");

    let output = collect_output_until(&mut *backend, Duration::from_secs(2), |text| {
        text.contains("TTY=1") || text.contains("TTY=0")
    });
    assert!(
        output.contains("TTY=1"),
        "expected tty-backed stdin, output was: {output:?}"
    );
}

#[test]
fn pty_roundtrip_input() {
    let factory = PtyPaneFactory;
    let mut backend = factory
        .spawn(&PaneSpawnConfig {
            shell: shell_path(),
            cwd: None,
            command: vec!["cat".to_string()],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
        })
        .expect("spawn backend");

    backend.write(b"hello from test\r").expect("write to pty");
    let output = collect_output_until(&mut *backend, Duration::from_secs(2), |text| {
        text.contains("hello from test")
    });

    assert!(
        output.contains("hello from test"),
        "expected echoed text from cat, output was: {output:?}"
    );
}

#[test]
fn interactive_shell_exit_is_detected_as_closed() {
    let factory = PtyPaneFactory;
    let mut backend = factory
        .spawn(&PaneSpawnConfig {
            shell: shell_path(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
        })
        .expect("spawn backend");

    backend.write(b"exit\r").expect("write exit command");

    assert!(
        wait_until_closed(&mut *backend, Duration::from_secs(3)),
        "expected backend to close after exit command"
    );
}
