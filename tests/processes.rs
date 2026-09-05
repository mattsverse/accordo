#![cfg(unix)]

use std::{
    fmt::Write as _,
    fs,
    path::Path,
    process::{Output, Stdio},
    time::Duration,
};

use accordo::{
    config::Service,
    pty::TerminalSize,
    supervisor::{Command, Event, EventKind, State, Stream, Supervisor},
};

async fn terminal_output(supervisor: &mut Supervisor) -> (Vec<u8>, State) {
    let mut bytes = Vec::new();
    loop {
        let event = next(supervisor).await;
        match event.kind {
            EventKind::Terminal { bytes: chunk, .. } => bytes.extend_from_slice(&chunk),
            EventKind::State(state) if event.run > 0 && state.is_finished() => {
                return (bytes, state);
            }
            EventKind::Output { .. } => panic!("terminal mode must preserve raw output"),
            EventKind::State(_) => {}
        }
    }
}

#[tokio::test]
async fn pty_exposes_a_terminal_and_preserves_ansi_and_final_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(
        directory.path(),
        "app",
        "[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || exit 23; stty size; printf '\\033[31mold\\r\\033[2Kready\\033[0m'",
        true,
    );
    let size = TerminalSize::new(4, 20);
    let mut supervisor = Supervisor::terminal(&[service], size);
    let (bytes, state) = terminal_output(&mut supervisor).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(state, State::Exited(Some(0)));
    assert!(bytes.starts_with(b"4 20\r\n"));
    assert!(bytes.ends_with(b"ready\x1b[0m"));
    let mut terminal = accordo::terminal::TerminalView::new(size);
    terminal.process(&bytes);
    assert_eq!(terminal.screen().contents(), "4 20\nready");
    assert_eq!(
        terminal.screen().cell(1, 0).unwrap().fgcolor(),
        tui_term::vt100::Color::Idx(1)
    );
}

#[tokio::test]
async fn pty_hides_stderr_before_merging_and_keeps_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    for hide in [true, false] {
        let mut service = service(
            directory.path(),
            "app",
            "printf 'stdout'; printf 'stderr' >&2; exit 7",
            true,
        );
        service.hide_stderr = hide;
        let mut supervisor = Supervisor::terminal(&[service], TerminalSize::new(4, 40));
        let (bytes, state) = terminal_output(&mut supervisor).await;
        supervisor.shutdown().await.unwrap();
        assert_eq!(state, State::Exited(Some(7)));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("stdout"));
        assert_eq!(text.contains("stderr"), !hide);
    }
}

#[tokio::test]
async fn pty_resize_reaches_running_and_not_yet_started_services() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(
        directory.path(),
        "app",
        "trap 'stty size > resized' WINCH; trap 'exit 0' TERM; stty size > initial; while :; do sleep 0.05; done",
        false,
    );
    let mut supervisor = Supervisor::terminal(&[service], TerminalSize::new(4, 20));
    supervisor.resize(TerminalSize::new(8, 40));
    supervisor.command(0, Command::Start);
    let initial = wait_file(&directory.path().join("initial")).await;
    supervisor.resize(TerminalSize::new(12, 60));
    let resized = wait_file(&directory.path().join("resized")).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(initial.trim(), "8 40");
    assert_eq!(resized.trim(), "12 60");
}

#[tokio::test]
async fn pty_restart_waits_for_the_old_process_to_release_resources() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(
        directory.path(),
        "app",
        "mkdir active || { touch overlap; exit 99; }; trap 'rmdir active; exit 0' TERM; echo $$ > run.pid; echo started >> runs; while :; do sleep 0.05; done",
        true,
    );
    let mut supervisor = Supervisor::terminal(&[service], TerminalSize::new(4, 20));
    let first_pid = wait_file(&directory.path().join("run.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    wait_file(&directory.path().join("runs")).await;
    supervisor.command(0, Command::Restart);
    wait_state(&mut supervisor, &State::Stopping).await;
    for _ in 0..20 {
        supervisor.command(0, Command::Restart);
    }
    let second = wait_state(&mut supervisor, &State::Running).await;
    timeout(Duration::from_secs(5), async {
        while fs::read_to_string(directory.path().join("runs"))
            .unwrap_or_default()
            .lines()
            .count()
            < 2
        {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    supervisor.shutdown().await.unwrap();
    assert_eq!(second.run, 2);
    assert!(!directory.path().join("overlap").exists());
    assert!(!directory.path().join("active").exists());
    assert_gone(first_pid).await;
}

#[tokio::test]
async fn pty_shutdown_kills_descendants_with_a_full_output_queue() {
    let directory = tempfile::tempdir().unwrap();
    let command = "trap '' TERM HUP; echo $$ > parent.pid; sh -c 'trap \"\" TERM HUP; echo $$ > child.pid; while :; do printf flooding-output; done' & wait";
    let mut supervisor = Supervisor::terminal(
        &[service(directory.path(), "app", command, true)],
        TerminalSize::new(4, 40),
    );
    let parent = wait_file(&directory.path().join("parent.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    let child = wait_file(&directory.path().join("child.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    sleep(Duration::from_millis(150)).await;
    timeout(Duration::from_secs(7), supervisor.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_gone(parent).await;
    assert_gone(child).await;
}

#[tokio::test]
async fn pty_natural_exit_cleans_descendants_and_keeps_final_output() {
    let directory = tempfile::tempdir().unwrap();
    let command = "sh -c 'trap \"\" TERM HUP; echo $$ > child.pid; while :; do sleep 1; done' & while [ ! -s child.pid ]; do sleep 0.01; done; printf final";
    let mut supervisor = Supervisor::terminal(
        &[service(directory.path(), "app", command, true)],
        TerminalSize::new(4, 20),
    );
    let child = wait_file(&directory.path().join("child.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    let (bytes, state) = terminal_output(&mut supervisor).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(state, State::Exited(Some(0)));
    assert!(String::from_utf8_lossy(&bytes).contains("final"));
    assert_gone(child).await;
}

#[tokio::test]
async fn pty_answers_device_status_queries_without_forwarding_accordo_keys() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(
        directory.path(),
        "app",
        "stty -echo -icanon min 1 time 0; printf '\\033[5n'; dd bs=1 count=4 of=reply 2>/dev/null; printf done",
        true,
    );
    let size = TerminalSize::new(4, 20);
    let mut supervisor = Supervisor::terminal(std::slice::from_ref(&service), size);
    let mut app = accordo::app::App::terminal(&[service], size);
    let state = loop {
        let event = next(&mut supervisor).await;
        let run = event.run;
        let final_state = match &event.kind {
            EventKind::State(state) if run > 0 && state.is_finished() => Some(state.clone()),
            _ => None,
        };
        app.apply(event);
        supervisor.reply(
            0,
            run,
            app.services[0].terminal.as_mut().unwrap().take_replies(),
        );
        if let Some(state) = final_state {
            break state;
        }
    };
    supervisor.shutdown().await.unwrap();
    assert_eq!(state, State::Exited(Some(0)));
    assert_eq!(
        fs::read(directory.path().join("reply")).unwrap(),
        b"\x1b[0n"
    );
    assert!(
        app.services[0]
            .terminal
            .as_ref()
            .unwrap()
            .screen()
            .contents()
            .contains("done")
    );
}
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command as ProcessCommand},
    time::{Instant, sleep, timeout},
};

fn service(directory: &Path, name: &str, command: &str, autostart: bool) -> Service {
    Service {
        name: name.into(),
        cmd: command.into(),
        cwd: directory.into(),
        autostart,
        hide_stderr: false,
    }
}

fn configuration(directory: &Path, services: &[Service], layout: &str) -> std::path::PathBuf {
    let mut yaml = format!("layout: {layout}\nservices:\n");
    for service in services {
        writeln!(
            yaml,
            "  - name: {}\n    autostart: {}\n    hide_stderr: {}\n    cmd: |",
            service.name, service.autostart, service.hide_stderr
        )
        .unwrap();
        for line in service.cmd.lines() {
            writeln!(yaml, "      {line}").unwrap();
        }
    }
    let path = directory.join("accordo.yaml");
    fs::write(&path, yaml).unwrap();
    path
}

fn accordo(path: &Path) -> Child {
    ProcessCommand::new(env!("CARGO_BIN_EXE_accordo"))
        .arg("-f")
        .arg(path)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

fn pid(child: &Child) -> Pid {
    Pid::from_raw(i32::try_from(child.id().unwrap()).unwrap())
}

async fn output(child: Child) -> Output {
    let id = pid(&child);
    let result = child.wait_with_output();
    tokio::pin!(result);
    if let Ok(result) = timeout(Duration::from_secs(15), &mut result).await {
        result.unwrap()
    } else {
        let _ = kill(id, Signal::SIGTERM);
        let _ = timeout(Duration::from_secs(6), result).await;
        panic!("Accordo did not finish within 15 seconds");
    }
}

async fn next(supervisor: &mut Supervisor) -> Event {
    timeout(Duration::from_secs(10), supervisor.events.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn wait_state(supervisor: &mut Supervisor, expected: &State) -> Event {
    loop {
        let event = next(supervisor).await;
        if matches!(&event.kind, EventKind::State(state) if state == expected) {
            return event;
        }
    }
}

async fn wait_file(path: &Path) -> String {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path)
                && !contents.trim().is_empty()
            {
                return contents;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

async fn assert_gone(id: i32) {
    timeout(Duration::from_secs(5), async {
        loop {
            if kill(Pid::from_raw(id), None) == Err(Errno::ESRCH) {
                break;
            }
            // Linux containers may not reap orphan zombies promptly; a zombie
            // has exited and cannot run or retain an output descriptor.
            #[cfg(target_os = "linux")]
            if fs::read_to_string(format!("/proc/{id}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, rest)| rest.starts_with('Z'))
            }) {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("service process survived cleanup");
}

#[tokio::test]
async fn linear_merges_streams_preserves_final_output_and_isolates_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = configuration(
        directory.path(),
        &[
            service(
                directory.path(),
                "bad",
                "printf 'out\\n'; printf '\\033[31merr\\033[0m\\n' >&2; exit 7",
                true,
            ),
            service(
                directory.path(),
                "good",
                "sleep 0.1; pwd; printf 'final'",
                true,
            ),
            service(
                directory.path(),
                "disabled",
                "touch should-not-exist",
                false,
            ),
        ],
        "linear",
    );
    let result = output(accordo(&path)).await;
    let text = String::from_utf8(result.stdout).unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(text.contains("[0 bad] out\n"), "{text}");
    assert!(text.contains("[0 bad] [stderr] err\n"));
    assert!(text.contains(&format!(
        "[1 good] {}\n",
        fs::canonicalize(directory.path()).unwrap().display()
    )));
    assert!(
        text.find("[1 good] final\n").unwrap() < text.find("[1 good] --- exited (0) ---").unwrap()
    );
    assert!(!text.contains('\x1b'));
    assert!(!directory.path().join("should-not-exist").exists());
}

#[tokio::test]
async fn hiding_stderr_is_per_service_and_drains_large_output() {
    let directory = tempfile::tempdir().unwrap();
    let mut hidden = service(
        directory.path(),
        "hidden",
        "printf 'keep-stdout\\n'; i=0; while [ $i -lt 5000 ]; do printf 'hidden-error-padding-to-fill-the-output-pipe\\n' >&2; i=$((i+1)); done; printf 'hidden-tail' >&2; exit 7",
        true,
    );
    hidden.hide_stderr = true;
    let visible = service(
        directory.path(),
        "visible",
        "printf 'keep-stderr\\n' >&2; echo keep-other-stdout",
        true,
    );
    let path = configuration(directory.path(), &[hidden, visible], "linear");
    let result = output(accordo(&path)).await;
    let text = String::from_utf8(result.stdout).unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(text.contains("[0 hidden] keep-stdout\n"));
    assert!(text.contains("[1 visible] [stderr] keep-stderr\n"));
    assert!(text.contains("[1 visible] keep-other-stdout\n"));
    assert!(text.contains("[0 hidden] --- exited (7) ---"));
    assert!(!text.contains("hidden-error"));
    assert!(!text.contains("hidden-tail"));
}

#[tokio::test]
async fn services_start_concurrently_and_preserve_each_streams_order() {
    let directory = tempfile::tempdir().unwrap();
    let services = [
        service(
            directory.path(),
            "a",
            "touch a.ready; while [ ! -f b.ready ]; do sleep 0.01; done; i=0; while [ $i -lt 50 ]; do echo out-$i; echo err-$i >&2; i=$((i+1)); done",
            true,
        ),
        service(
            directory.path(),
            "b",
            "touch b.ready; while [ ! -f a.ready ]; do sleep 0.01; done; i=0; while [ $i -lt 50 ]; do echo out-$i; echo err-$i >&2; i=$((i+1)); done",
            true,
        ),
    ];
    let mut supervisor = Supervisor::new(&services);
    let mut records = [[Vec::new(), Vec::new()], [Vec::new(), Vec::new()]];
    let mut complete = 0;
    while complete < 2 {
        let event = next(&mut supervisor).await;
        match event.kind {
            EventKind::Output { stream, text, .. } => {
                records[event.service][usize::from(stream == Stream::Stderr)].push(text);
            }
            EventKind::State(State::Exited(Some(0))) => complete += 1,
            EventKind::State(_) => {}
            EventKind::Terminal { .. } => panic!("linear mode must emit log records"),
        }
    }
    supervisor.shutdown().await.unwrap();
    for streams in records {
        assert_eq!(
            streams[0],
            (0..50)
                .map(|index| format!("out-{index}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            streams[1],
            (0..50)
                .map(|index| format!("err-{index}"))
                .collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn validates_every_service_before_starting_and_requires_tty_for_tabs() {
    let directory = tempfile::tempdir().unwrap();
    let path = configuration(
        directory.path(),
        &[
            service(directory.path(), "first", "touch marker", true),
            service(directory.path(), "invalid", " ", true),
        ],
        "linear",
    );
    let result = output(accordo(&path)).await;
    assert_eq!(result.status.code(), Some(1));
    assert!(!directory.path().join("marker").exists());
    let path = configuration(
        directory.path(),
        &[service(directory.path(), "first", "touch marker", true)],
        "tabbed",
    );
    let result = output(accordo(&path)).await;
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("layout: linear"));
    assert!(!directory.path().join("marker").exists());
}

#[tokio::test]
async fn default_filename_no_enabled_services_and_command_not_found() {
    let directory = tempfile::tempdir().unwrap();
    configuration(
        directory.path(),
        &[service(directory.path(), "disabled", "touch marker", false)],
        "linear",
    );
    let child = ProcessCommand::new(env!("CARGO_BIN_EXE_accordo"))
        .current_dir(directory.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let result = output(child).await;
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("No services enabled"));
    assert!(!directory.path().join("marker").exists());
    let path = configuration(
        directory.path(),
        &[service(
            directory.path(),
            "missing",
            "accordo-command-that-does-not-exist",
            true,
        )],
        "linear",
    );
    let result = output(accordo(&path)).await;
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stdout).contains("exited (127)"));
}

#[tokio::test]
async fn restart_waits_for_cleanup_and_rapid_requests_do_not_overlap() {
    let directory = tempfile::tempdir().unwrap();
    let command = "if ! mkdir active; then echo overlap; exit 99; fi; trap 'rmdir active; exit 0' TERM; echo ready; while :; do sleep 0.05; done";
    let mut supervisor = Supervisor::new(&[service(directory.path(), "app", command, false)]);
    wait_state(&mut supervisor, &State::Stopped).await;
    supervisor.command(0, Command::Start);
    let first = loop {
        let event = next(&mut supervisor).await;
        if matches!(&event.kind, EventKind::Output { text, .. } if text == "ready") {
            break event.run;
        }
    };
    supervisor.command(0, Command::Restart);
    wait_state(&mut supervisor, &State::Stopping).await;
    for _ in 0..30 {
        supervisor.command(0, Command::Restart);
    }
    let mut overlap = false;
    let second = loop {
        let event = next(&mut supervisor).await;
        if let EventKind::Output { text, .. } = &event.kind {
            overlap |= text == "overlap";
            if text == "ready" {
                break event.run;
            }
        }
    };
    supervisor.command(0, Command::Stop);
    wait_state(&mut supervisor, &State::Stopped).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(second, first + 1);
    assert!(!overlap);
    assert!(!directory.path().join("active").exists());
}

#[tokio::test]
async fn shutdown_kills_ignoring_descendants_even_when_output_is_full() {
    let directory = tempfile::tempdir().unwrap();
    let command = "trap '' TERM; echo $$ > parent.pid; sh -c 'trap \"\" TERM; echo $$ > child.pid; while :; do printf flood; done' & wait";
    let mut supervisor = Supervisor::new(&[service(directory.path(), "app", command, true)]);
    let parent = wait_file(&directory.path().join("parent.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    let child = wait_file(&directory.path().join("child.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    // Intentionally don't consume output before requesting shutdown.
    sleep(Duration::from_millis(150)).await;
    let started = Instant::now();
    timeout(Duration::from_secs(7), supervisor.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert_gone(parent).await;
    assert_gone(child).await;
}

#[tokio::test]
async fn natural_shell_exit_cleans_remaining_children() {
    let directory = tempfile::tempdir().unwrap();
    let command = "sh -c 'trap \"\" TERM; echo $$ > child.pid; while :; do sleep 1; done' & while [ ! -s child.pid ]; do sleep 0.01; done; echo final";
    let mut supervisor = Supervisor::new(&[service(directory.path(), "app", command, true)]);
    let child = wait_file(&directory.path().join("child.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    let mut final_seen = false;
    loop {
        let event = next(&mut supervisor).await;
        match event.kind {
            EventKind::Output { text, .. } if text == "final" => final_seen = true,
            EventKind::State(State::Exited(Some(0))) => break,
            _ => {}
        }
    }
    supervisor.shutdown().await.unwrap();
    assert!(final_seen);
    assert_gone(child).await;
}

#[tokio::test]
async fn slow_consumer_keeps_all_final_output() {
    let directory = tempfile::tempdir().unwrap();
    let command =
        "i=0; while [ \"$i\" -lt 1500 ]; do echo \"$i\"; i=$((i+1)); done; printf final >&2";
    let mut supervisor = Supervisor::new(&[service(directory.path(), "app", command, true)]);
    sleep(Duration::from_millis(2300)).await;
    let mut count = 0;
    let mut final_seen = false;
    loop {
        let event = next(&mut supervisor).await;
        match event.kind {
            EventKind::Output {
                stream: Stream::Stdout,
                ..
            } => count += 1,
            EventKind::Output {
                stream: Stream::Stderr,
                text,
                ..
            } => final_seen |= text == "final",
            EventKind::State(State::Exited(Some(0))) => break,
            _ => {}
        }
    }
    supervisor.shutdown().await.unwrap();
    assert_eq!(count, 1500);
    assert!(final_seen);
}

#[tokio::test]
async fn sigint_cleans_services_with_a_blocked_output_pipe() {
    let directory = tempfile::tempdir().unwrap();
    let path = configuration(
        directory.path(),
        &[service(
            directory.path(),
            "app",
            "echo $$ > service.pid; while :; do echo output; done",
            true,
        )],
        "linear",
    );
    let mut child = accordo(&path);
    let service_pid = wait_file(&directory.path().join("service.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    sleep(Duration::from_millis(150)).await;
    kill(pid(&child), Signal::SIGINT).unwrap();
    // Retain but never read stdout: the writer may be blocked in the kernel.
    let result = timeout(Duration::from_secs(7), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.code(), Some(130));
    assert_gone(service_pid).await;
}

#[tokio::test]
async fn broken_pipe_and_sigterm_trigger_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    for signal in [None, Some(Signal::SIGTERM)] {
        let _ = fs::remove_file(directory.path().join("service.pid"));
        let path = configuration(
            directory.path(),
            &[service(
                directory.path(),
                "app",
                "echo $$ > service.pid; while :; do echo output; done",
                true,
            )],
            "linear",
        );
        let mut child = accordo(&path);
        let service_pid = wait_file(&directory.path().join("service.pid"))
            .await
            .trim()
            .parse()
            .unwrap();
        if let Some(signal) = signal {
            kill(pid(&child), signal).unwrap();
        } else {
            drop(child.stdout.take());
        }
        let result = timeout(Duration::from_secs(7), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.code(), Some(if signal.is_some() { 143 } else { 0 }));
        assert_gone(service_pid).await;
    }
}

#[tokio::test]
async fn spawn_failure_is_reported_without_losing_other_services() {
    let directory = tempfile::tempdir().unwrap();
    let mut supervisor = Supervisor::new(&[
        service(&directory.path().join("missing"), "bad", "true", true),
        service(directory.path(), "good", "echo okay", true),
    ]);
    let mut failed = false;
    let mut good = false;
    while !(failed && good) {
        let event = next(&mut supervisor).await;
        failed |= event.service == 0 && matches!(event.kind, EventKind::State(State::Failed(_)));
        good |=
            event.service == 1 && matches!(event.kind, EventKind::State(State::Exited(Some(0))));
    }
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn linear_ignores_q_on_stdin() {
    let directory = tempfile::tempdir().unwrap();
    let path = configuration(
        directory.path(),
        &[service(
            directory.path(),
            "app",
            "sleep 0.1; echo complete",
            true,
        )],
        "linear",
    );
    let input = directory.path().join("input");
    fs::write(&input, "q\ns\nr\n").unwrap();
    let child = ProcessCommand::new(env!("CARGO_BIN_EXE_accordo"))
        .arg("-f")
        .arg(path)
        .stdin(fs::File::open(input).unwrap())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let result = output(child).await;
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("complete"));
}

#[tokio::test]
async fn tabbed_stdin_is_not_passed_to_children() {
    // Child input behavior is shared by both frontends.
    let directory = tempfile::tempdir().unwrap();
    let path = configuration(
        directory.path(),
        &[service(
            directory.path(),
            "app",
            "if read -r value; then echo unexpected-input; else echo stdin-closed; fi",
            true,
        )],
        "linear",
    );
    let mut child = accordo(&path);
    let mut stdout = child.stdout.take().unwrap();
    let mut text = String::new();
    timeout(Duration::from_secs(5), stdout.read_to_string(&mut text))
        .await
        .unwrap()
        .unwrap();
    assert!(child.wait().await.unwrap().success());
    assert!(text.contains("stdin-closed"));
}
