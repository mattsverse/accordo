use std::{io, process::Stdio, sync::Arc, time::Duration};

use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command as ProcessCommand},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep},
};

use crate::{
    config::Service,
    logs::Decoder,
    pty::{PtyIo, Session, TerminalSize},
};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const OUTPUT_CAPACITY: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Start,
    Stop,
    Restart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    Starting,
    Running,
    Stopping,
    Exited(Option<i32>),
    Failed(String),
}

impl State {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }

    #[must_use]
    pub const fn is_finished(&self) -> bool {
        matches!(self, Self::Stopped | Self::Exited(_) | Self::Failed(_))
    }

    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Stopped => "stopped".into(),
            Self::Starting => "starting".into(),
            Self::Running => "running".into(),
            Self::Stopping => "stopping".into(),
            Self::Exited(Some(code)) => format!("exited ({code})"),
            Self::Exited(None) => "exited (signal)".into(),
            Self::Failed(error) => format!("failed: {error}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub enum EventKind {
    State(State),
    Terminal {
        bytes: Vec<u8>,
        permit: OwnedSemaphorePermit,
    },
    Output {
        stream: Stream,
        text: String,
        // Only output consumes permits; control and lifecycle events cannot be
        // starved by a child filling its output pipe.
        permit: OwnedSemaphorePermit,
    },
}

#[derive(Debug)]
pub struct Event {
    pub service: usize,
    pub run: u64,
    pub kind: EventKind,
}

#[derive(Clone)]
struct Emitter {
    service: usize,
    run: u64,
    events: mpsc::UnboundedSender<Event>,
    capacity: Arc<Semaphore>,
}

impl Emitter {
    fn state(&self, state: State) {
        let _ = self.events.send(Event {
            service: self.service,
            run: self.run,
            kind: EventKind::State(state),
        });
    }

    async fn terminal(&self, bytes: Vec<u8>) -> bool {
        let Ok(permit) = self.capacity.clone().acquire_owned().await else {
            return false;
        };
        self.events
            .send(Event {
                service: self.service,
                run: self.run,
                kind: EventKind::Terminal { bytes, permit },
            })
            .is_ok()
    }

    async fn output(&self, stream: Stream, text: String) -> bool {
        let Ok(permit) = self.capacity.clone().acquire_owned().await else {
            return false;
        };
        self.events
            .send(Event {
                service: self.service,
                run: self.run,
                kind: EventKind::Output {
                    stream,
                    text,
                    permit,
                },
            })
            .is_ok()
    }
}

pub struct Supervisor {
    pub events: mpsc::UnboundedReceiver<Event>,
    commands: Vec<mpsc::Sender<Command>>,
    shutdown: watch::Sender<bool>,
    tasks: JoinSet<()>,
    size: Option<watch::Sender<TerminalSize>>,
    replies: Vec<mpsc::Sender<(u64, Vec<u8>)>>,
}

impl Supervisor {
    #[must_use]
    pub fn new(services: &[Service]) -> Self {
        Self::create(services, None)
    }

    #[must_use]
    pub fn terminal(services: &[Service], size: TerminalSize) -> Self {
        Self::create(services, Some(size))
    }

    fn create(services: &[Service], size: Option<TerminalSize>) -> Self {
        let size = size.map(|size| watch::channel(size).0);
        let mut replies = Vec::new();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let capacity = Arc::new(Semaphore::new(OUTPUT_CAPACITY));
        let mut commands = Vec::new();
        let mut tasks = JoinSet::new();
        for (id, service) in services.iter().cloned().enumerate() {
            let (tx, rx) = mpsc::channel(1);
            commands.push(tx);
            let emitter = Emitter {
                service: id,
                run: 0,
                events: events_tx.clone(),
                capacity: capacity.clone(),
            };
            let (reply_tx, reply_rx) = mpsc::channel(16);
            replies.push(reply_tx);
            tasks.spawn(service_actor(
                service,
                rx,
                shutdown_rx.clone(),
                emitter,
                size.as_ref().map(watch::Sender::subscribe),
                reply_rx,
            ));
        }
        Self {
            events,
            commands,
            shutdown,
            tasks,
            size,
            replies,
        }
    }

    pub fn command(&self, service: usize, command: Command) {
        if let Some(sender) = self.commands.get(service) {
            // Coalesce rapid key presses rather than building a restart queue.
            let _ = sender.try_send(command);
        }
    }

    pub fn resize(&self, size: TerminalSize) {
        if let Some(sender) = &self.size {
            sender.send_if_modified(|current| {
                if *current == size {
                    false
                } else {
                    *current = size;
                    true
                }
            });
        }
    }

    pub fn reply(&self, service: usize, run: u64, bytes: Vec<u8>) {
        if !bytes.is_empty()
            && let Some(sender) = self.replies.get(service)
        {
            let _ = sender.try_send((run, bytes));
        }
    }

    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Stop all actors, draining output so a full queue cannot block cleanup.
    ///
    /// # Errors
    /// Returns an error if a supervisor task panicked.
    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.request_shutdown();
        let mut error = None;
        while !self.tasks.is_empty() {
            tokio::select! {
                result = self.tasks.join_next() => {
                    if let Some(Err(failure)) = result {
                        error = Some(format!("process supervisor failed: {failure}"));
                    }
                }
                Some(_) = self.events.recv() => {}
            }
        }
        error.map_or(Ok(()), Err)
    }
}

async fn service_actor(
    service: Service,
    mut commands: mpsc::Receiver<Command>,
    mut shutdown: watch::Receiver<bool>,
    mut emitter: Emitter,
    mut size: Option<watch::Receiver<TerminalSize>>,
    mut replies: mpsc::Receiver<(u64, Vec<u8>)>,
) {
    let mut start = service.autostart;
    emitter.state(State::Stopped);
    loop {
        if *shutdown.borrow() {
            break;
        }
        if !start {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                command = commands.recv() => match command {
                    Some(Command::Start | Command::Restart) => {},
                    Some(Command::Stop) => continue,
                    None => break,
                },
            }
        }
        if *shutdown.borrow() {
            break;
        }
        emitter.run += 1;
        emitter.state(State::Starting);
        start = run_service(
            &service,
            &mut commands,
            &mut shutdown,
            &emitter,
            &mut size,
            &mut replies,
        )
        .await;
    }
}

async fn run_service(
    service: &Service,
    commands: &mut mpsc::Receiver<Command>,
    shutdown: &mut watch::Receiver<bool>,
    emitter: &Emitter,
    size: &mut Option<watch::Receiver<TerminalSize>>,
    replies: &mut mpsc::Receiver<(u64, Vec<u8>)>,
) -> bool {
    let result = RunningProcess::spawn(service, size.as_ref().map(|rx| *rx.borrow()));
    let mut child = match result {
        Ok(child) => child,
        Err(error) => {
            while commands.try_recv().is_ok() {}
            emitter.state(State::Failed(error.to_string()));
            return false;
        }
    };
    // Unix process IDs fit pid_t, which is i32 on supported platforms.
    let group = Pid::from_raw(
        i32::try_from(child.id().expect("new child has a PID")).expect("PID fits pid_t"),
    );
    let mut guard = ProcessGroupGuard(Some(group));
    let (finished, finished_rx) = watch::channel(false);
    let readers = match &mut child {
        RunningProcess::Pipe(child) => vec![
            tokio::spawn(read_output(
                child.stdout.take().expect("stdout is piped"),
                Stream::Stdout,
                emitter.clone(),
                finished_rx.clone(),
            )),
            tokio::spawn(read_output(
                child.stderr.take().expect("stderr is piped"),
                Stream::Stderr,
                emitter.clone(),
                finished_rx,
            )),
        ],
        RunningProcess::Terminal(session) => vec![tokio::spawn(read_terminal(
            session.io.clone(),
            emitter.clone(),
            finished_rx,
        ))],
    };
    emitter.state(State::Running);

    let (status, restart, stopped) = loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break (None, false, true),
            command = commands.recv() => match command {
                Some(Command::Start) => {},
                Some(Command::Restart) => break (None, true, true),
                Some(Command::Stop) | None => break (None, false, true),
            },
            changed = next_size(size) => {
                if let Err(error) = child.resize(changed) { break (Some(Err(error)), false, false); }
            },
            Some((run, bytes)) = replies.recv() => {
                if run == emitter.run && let RunningProcess::Terminal(session) = &child {
                    let _ = tokio::time::timeout(Duration::from_millis(50), session.io.reply(&bytes)).await;
                }
            },
            status = child.wait() => break (Some(status), false, false),
        }
    };
    if stopped {
        emitter.state(State::Stopping);
    }
    // Even when the shell exits naturally, descendants may still own its pipes.
    let result = cleanup(&mut child, group, status).await;
    if result.is_ok() {
        guard.0 = None;
    }
    drop(guard);
    let _ = finished.send(true);
    let output_result = finish_readers(readers).await;
    // All readers finish enqueueing before the terminal state is emitted. A
    // single event queue therefore preserves final output before completion.
    let state = match (result, output_result) {
        (Err(error), _) | (_, Err(error)) => State::Failed(error.to_string()),
        (Ok(_), Ok(())) if stopped => State::Stopped,
        (Ok(status), Ok(())) => State::Exited(status.code),
    };
    // Drain before publishing completion, so a command issued in response to
    // the completed state cannot be accidentally discarded.
    while commands.try_recv().is_ok() {}
    emitter.state(state);
    restart && !*shutdown.borrow()
}

struct ProcessGroupGuard(Option<Pid>);

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        // Best effort fallback on task cancellation/unwinding, as well as cleanup
        // errors. Normal shutdown also explicitly waits for the direct child.
        if let Some(group) = self.0 {
            let _ = killpg(group, Signal::SIGKILL);
        }
    }
}

fn signal_group(group: Pid, signal: Signal) -> io::Result<()> {
    match killpg(group, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
    }
}

fn group_exists(group: Pid) -> bool {
    !matches!(killpg(group, None), Err(Errno::ESRCH))
}

async fn cleanup(
    child: &mut RunningProcess,
    group: Pid,
    status: Option<io::Result<ProcessExit>>,
) -> io::Result<ProcessExit> {
    signal_group(group, Signal::SIGTERM)?;
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    let (mut status, failure) = match status {
        Some(Ok(status)) => (Some(status), None),
        Some(Err(error)) => (None, Some(error)),
        None => (None, None),
    };
    while group_exists(group) && Instant::now() < deadline {
        if status.is_none() {
            status = child.try_wait()?;
        }
        sleep(Duration::from_millis(25)).await;
    }
    if group_exists(group) {
        signal_group(group, Signal::SIGKILL)?;
    }
    let status = match status {
        Some(status) => status,
        None => child.wait().await?,
    };
    failure.map_or(Ok(status), Err)
}

async fn read_output<R: AsyncRead + Unpin>(
    mut reader: R,
    stream: Stream,
    emitter: Emitter,
    mut finished: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut decoder = Decoder::default();
    let mut buffer = [0_u8; 4096];
    let mut process_finished = false;
    loop {
        // Time out only an idle pipe after process cleanup, never time spent
        // waiting for output capacity. Slow consumers must receive final logs.
        let count = if process_finished {
            tokio::time::timeout(Duration::from_secs(2), reader.read(&mut buffer))
                .await
                .map_err(|_| {
                    io::Error::other("output pipe remained open after service stopped")
                })??
        } else {
            tokio::select! {
                count = reader.read(&mut buffer) => count?,
                _ = finished.changed() => { process_finished = true; continue; }
            }
        };
        if count == 0 {
            break;
        }
        for record in decoder.push(&buffer[..count]) {
            if !emitter.output(stream, record).await {
                return Ok(());
            }
        }
    }
    if let Some(record) = decoder.finish() {
        emitter.output(stream, record).await;
    }
    Ok(())
}

async fn finish_readers(readers: Vec<JoinHandle<io::Result<()>>>) -> io::Result<()> {
    let mut error = None;
    for reader in readers {
        if let Err(failure) = reader
            .await
            .map_err(io::Error::other)
            .and_then(|result| result)
        {
            error = Some(failure);
        }
    }
    error.map_or(Ok(()), Err)
}

async fn next_size(size: &mut Option<watch::Receiver<TerminalSize>>) -> TerminalSize {
    if let Some(size) = size
        && size.changed().await.is_ok()
    {
        return *size.borrow_and_update();
    }
    std::future::pending().await
}

async fn read_terminal(
    io: Arc<PtyIo>,
    emitter: Emitter,
    mut finished: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut buffer = [0_u8; 4096];
    let mut process_finished = false;
    loop {
        let count = if process_finished {
            tokio::time::timeout(Duration::from_secs(2), io.read(&mut buffer))
                .await
                .map_err(|_| io::Error::other("PTY remained open after service stopped"))??
        } else {
            tokio::select! {
                count = io.read(&mut buffer) => count?,
                _ = finished.changed() => { process_finished = true; continue; }
            }
        };
        if count == 0 {
            return Ok(());
        }
        if !emitter.terminal(buffer[..count].to_vec()).await {
            return Ok(());
        }
    }
}

struct ProcessExit {
    code: Option<i32>,
}

enum RunningProcess {
    Pipe(Child),
    Terminal(Session),
}

impl RunningProcess {
    fn spawn(service: &Service, size: Option<TerminalSize>) -> io::Result<Self> {
        if let Some(size) = size {
            return Session::spawn(service, size).map(Self::Terminal);
        }
        ProcessCommand::new("/bin/sh")
            .arg("-c")
            .arg(&service.cmd)
            .current_dir(&service.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map(Self::Pipe)
    }

    fn id(&self) -> Option<u32> {
        match self {
            Self::Pipe(child) => child.id(),
            Self::Terminal(session) => session.child.process_id(),
        }
    }

    fn resize(&self, size: TerminalSize) -> io::Result<()> {
        match self {
            Self::Pipe(_) => Ok(()),
            Self::Terminal(session) => session.resize(size),
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ProcessExit>> {
        match self {
            Self::Pipe(child) => child.try_wait().map(|status| {
                status.map(|status| ProcessExit {
                    code: status.code(),
                })
            }),
            Self::Terminal(session) => session.child.try_wait().map(|status| {
                status.map(|status| ProcessExit {
                    code: if status.signal().is_some() {
                        None
                    } else {
                        i32::try_from(status.exit_code()).ok()
                    },
                })
            }),
        }
    }

    async fn wait(&mut self) -> io::Result<ProcessExit> {
        if let Self::Pipe(child) = self {
            return child.wait().await.map(|status| ProcessExit {
                code: status.code(),
            });
        }
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            sleep(Duration::from_millis(25)).await;
        }
    }
}
