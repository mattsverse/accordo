use std::{
    io::{self, Write},
    time::Duration,
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as TerminalEvent, MouseButton,
        MouseEventKind,
    },
    execute,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph},
};
use tokio::{
    signal::unix::{Signal, SignalKind, signal},
    sync::{mpsc, oneshot},
    time::{MissedTickBehavior, interval},
};

use crate::{
    app::{Action, App},
    pty::TerminalSize,
    supervisor::{State, Supervisor},
};

pub struct Signals {
    interrupt: Signal,
    terminate: Signal,
}

impl Signals {
    /// Install signal handlers before spawning any services.
    ///
    /// # Errors
    /// Returns an error if the operating system cannot install a handler.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn receive(&mut self) -> u8 {
        tokio::select! {
            _ = self.interrupt.recv() => 130,
            _ = self.terminate.recv() => 143,
        }
    }
}

struct WriteRequest {
    line: String,
    done: oneshot::Sender<io::Result<()>>,
}

struct LinearWriter(mpsc::Sender<WriteRequest>);

impl LinearWriter {
    fn new() -> io::Result<Self> {
        let (sender, mut receiver) = mpsc::channel::<WriteRequest>(1);
        // A dedicated OS thread keeps a blocked output pipe from blocking signal
        // handling or the async runtime's shutdown. It owns no service resources.
        std::thread::Builder::new()
            .name("accordo-output".into())
            .spawn(move || {
                let mut stdout = io::stdout().lock();
                while let Some(request) = receiver.blocking_recv() {
                    let result = writeln!(stdout, "{}", request.line).and_then(|()| stdout.flush());
                    let failed = result.is_err();
                    let _ = request.done.send(result);
                    if failed {
                        break;
                    }
                }
            })?;
        Ok(Self(sender))
    }

    async fn line(&self, line: String) -> io::Result<()> {
        let (done, result) = oneshot::channel();
        self.0
            .send(WriteRequest { line, done })
            .await
            .map_err(|_| io::Error::other("output writer stopped"))?;
        result
            .await
            .map_err(|_| io::Error::other("output writer stopped"))?
    }
}

/// Run ordinary prefixed terminal output, with no keyboard bindings.
///
/// # Errors
/// Returns an error for output failures other than a closed pipe.
pub async fn linear(
    app: &mut App,
    supervisor: &mut Supervisor,
    signals: &mut Signals,
) -> io::Result<u8> {
    let writer = LinearWriter::new()?;
    tokio::select! {
        biased;
        code = signals.receive() => Ok(code),
        result = linear_events(app, supervisor, &writer) => match result {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(0),
            other => other,
        },
    }
}

async fn linear_events(
    app: &mut App,
    supervisor: &mut Supervisor,
    writer: &LinearWriter,
) -> io::Result<u8> {
    if app.services.iter().all(|service| !service.autostart) {
        writer
            .line("No services enabled (all services have autostart: false).".into())
            .await?;
        return Ok(0);
    }
    while let Some(event) = supervisor.events.recv().await {
        if let Some(line) = app.apply(event) {
            writer.line(line).await?;
        }
        if app.all_enabled_finished() {
            return Ok(u8::from(app.failed));
        }
    }
    Err(io::Error::other("process supervisor stopped unexpectedly"))
}

pub struct TerminalGuard {
    pub terminal: DefaultTerminal,
}

impl TerminalGuard {
    /// Enter the alternate screen and raw input mode.
    ///
    /// # Errors
    /// Returns an error if terminal initialization fails, restoring partial setup.
    pub fn new() -> io::Result<Self> {
        match ratatui::try_init() {
            Ok(terminal) => {
                let mut guard = Self { terminal };
                execute!(guard.terminal.backend_mut(), EnableMouseCapture)?;
                Ok(guard)
            }
            Err(error) => {
                ratatui::restore();
                Err(error)
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        ratatui::restore();
    }
}

/// Render and interact with the tabbed view until an exit request.
///
/// # Errors
/// Returns terminal input or drawing errors.
pub async fn tabbed(
    app: &mut App,
    supervisor: &mut Supervisor,
    signals: &mut Signals,
    terminal: &mut DefaultTerminal,
) -> io::Result<u8> {
    let mut ticker = interval(Duration::from_millis(33));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            code = signals.receive() => return Ok(code),
            _ = ticker.tick() => {
                let size = terminal.size()?;
                let area = Rect::new(0, 0, size.width, size.height);
                let size = terminal_size(area);
                app.resize_terminal(size);
                supervisor.resize(size);
                // Bound each input batch so event floods cannot starve rendering.
                for _ in 0..32 {
                    if !event::poll(Duration::ZERO)? { break; }
                    match handle_input(app, &event::read()?, area) {
                        Some(Action::Quit(code)) => return Ok(code),
                        Some(Action::Service(id, command)) => supervisor.command(id, command),
                        None => {},
                    }
                }
                terminal.draw(|frame| render(frame, app))?;
            }
            event = supervisor.events.recv() => {
                let Some(event) = event else { return Err(io::Error::other("process supervisor stopped unexpectedly")); };
                let (id, run) = (event.service, event.run);
                app.apply(event);
                if let Some(terminal) = app.services.get_mut(id).and_then(|service| service.terminal.as_mut()) {
                    supervisor.reply(id, run, terminal.take_replies());
                }
            }
        }
    }
}

struct Panes {
    sidebar: Rect,
    logs: Rect,
    footer: Rect,
}

fn panes(area: Rect) -> Panes {
    let vertical = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let horizontal = Layout::horizontal([
        Constraint::Length((area.width / 3).clamp(10, 30)),
        Constraint::Min(0),
    ])
    .split(vertical[0]);
    Panes {
        sidebar: horizontal[0],
        logs: horizontal[1],
        footer: vertical[1],
    }
}

#[must_use]
pub fn terminal_size(area: Rect) -> TerminalSize {
    let inner = Block::bordered().inner(panes(area).logs);
    TerminalSize::new(inner.height, inner.width)
}

fn sidebar_start(app: &App, height: u16) -> usize {
    app.selected
        .saturating_sub(usize::from(height.saturating_sub(1)))
}

fn handle_input(app: &mut App, event: &TerminalEvent, area: Rect) -> Option<Action> {
    match event {
        TerminalEvent::Key(key) => app.key(*key),
        TerminalEvent::Mouse(mouse) if area.width >= 24 && area.height >= 4 => {
            let panes = panes(area);
            let point = Position::new(mouse.column, mouse.row);
            let log_inner = Block::bordered().inner(panes.logs);
            let sidebar_inner = Block::bordered().inner(panes.sidebar);
            if log_inner.contains(point) {
                match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll_logs(true, 3),
                    MouseEventKind::ScrollDown => app.scroll_logs(false, 3),
                    _ => {}
                }
            } else if sidebar_inner.contains(point)
                && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            {
                let index = sidebar_start(app, sidebar_inner.height)
                    + usize::from(mouse.row - sidebar_inner.y);
                if index < app.services.len() {
                    app.selected = index;
                }
            }
            None
        }
        _ => None,
    }
}

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width < 24 || area.height < 4 {
        frame.render_widget(Paragraph::new("Accordo · enlarge terminal"), area);
        return;
    }
    let panes = panes(area);
    render_sidebar(frame, app, panes.sidebar);
    render_logs(frame, app, panes.logs);
    frame.render_widget(
        Paragraph::new(
            "↑↓/0–9 select · wheel/PgUp/Dn scroll · End live · s start/stop · r restart · q quit",
        )
        .dim(),
        panes.footer,
    );
}

fn render_sidebar(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::bordered().title(" Services ");
    let first = sidebar_start(app, block.inner(area).height);
    let items: Vec<ListItem<'_>> = app
        .services
        .iter()
        .enumerate()
        .skip(first)
        .map(|(id, service)| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{id} {} ", service.name)),
                Span::styled(service.state.label(), status_style(&service.state)),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.selected.saturating_sub(first)));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("› ")
            .highlight_style(Style::new().bold().bg(Color::DarkGray))
            .highlight_spacing(ratatui::widgets::HighlightSpacing::Always),
        area,
        &mut state,
    );
}

fn render_logs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if let Some(service) = app.services.get(app.selected) {
        let offset = service
            .terminal
            .as_ref()
            .map_or(0, crate::terminal::TerminalView::offset);
        let block = Block::bordered()
            .title(format!(" {} · {} ", service.name, service.state.label()))
            .title_bottom(if offset == 0 {
                " LIVE ".into()
            } else {
                format!(" ↑ {offset} lines · End: live ")
            });
        if let Some(terminal) = &service.terminal
            && terminal.received_output
        {
            frame.render_widget(
                tui_term::widget::PseudoTerminal::new(terminal.screen())
                    .block(block)
                    .cursor(
                        tui_term::widget::Cursor::default()
                            .visibility(service.state.is_active() && offset == 0),
                    ),
                area,
            );
        } else {
            frame.render_widget(
                Paragraph::new("No output yet. Press s to start a stopped service.")
                    .dim()
                    .block(block),
                area,
            );
        }
    }
}

fn status_style(state: &State) -> Style {
    Style::new().fg(match state {
        State::Running => Color::Green,
        State::Starting | State::Stopping => Color::Yellow,
        State::Failed(_) | State::Exited(None) => Color::Red,
        State::Exited(Some(code)) if *code != 0 => Color::Red,
        _ => Color::DarkGray,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Service;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn terminal_widget_renders_styles_and_cursor_redraws_inside_the_pane() {
        let services = [Service {
            name: "app".into(),
            cmd: "true".into(),
            cwd: ".".into(),
            autostart: false,
            hide_stderr: false,
        }];
        let area = Rect::new(0, 0, 80, 12);
        let mut app = App::terminal(&services, terminal_size(area));
        app.services[0]
            .terminal
            .as_mut()
            .unwrap()
            .process(b"old progress\r\x1b[2K\x1b[31mready\x1b[0m");
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let inner = Block::bordered().inner(panes(area).logs);
        let cell = &terminal.backend().buffer()[(inner.x, inner.y)];
        assert_eq!(cell.symbol(), "r");
        assert_eq!(cell.fg, Color::Indexed(1));
        let row = (inner.x..inner.right())
            .map(|column| terminal.backend().buffer()[(column, inner.y)].symbol())
            .collect::<String>();
        assert_eq!(row.trim(), "ready");
    }

    #[test]
    fn wheel_scrolls_visible_logs_without_changing_services() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
        let mut app = App::terminal(
            &[
                Service {
                    name: "first".into(),
                    cmd: "true".into(),
                    cwd: ".".into(),
                    autostart: false,
                    hide_stderr: false,
                },
                Service {
                    name: "second".into(),
                    cmd: "true".into(),
                    cwd: ".".into(),
                    autostart: false,
                    hide_stderr: false,
                },
            ],
            TerminalSize::new(9, 52),
        );
        app.services[0].terminal.as_mut().unwrap().process(
            (0..100)
                .map(|index| format!("line-{index:03}"))
                .collect::<Vec<_>>()
                .join("\r\n")
                .as_bytes(),
        );
        let area = Rect::new(0, 0, 80, 12);
        app.resize_terminal(terminal_size(area));
        let wheel = |kind, column| {
            TerminalEvent::Mouse(MouseEvent {
                kind,
                column,
                row: 2,
                modifiers: KeyModifiers::NONE,
            })
        };
        handle_input(&mut app, &wheel(MouseEventKind::ScrollUp, 50), area);
        assert_eq!(app.selected, 0);
        assert_eq!(app.services[0].terminal.as_ref().unwrap().offset(), 3);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("line-088"));
        assert!(!text.contains("line-099"));
        handle_input(&mut app, &wheel(MouseEventKind::ScrollUp, 2), area);
        assert_eq!(app.selected, 0);
        assert_eq!(app.services[0].terminal.as_ref().unwrap().offset(), 3);
        handle_input(&mut app, &wheel(MouseEventKind::ScrollDown, 50), area);
        assert_eq!(app.services[0].terminal.as_ref().unwrap().offset(), 0);
        handle_input(
            &mut app,
            &TerminalEvent::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            area,
        );
        assert_eq!(app.selected, 0);
        assert!(app.services[0].terminal.as_ref().unwrap().offset() > 0);
        handle_input(
            &mut app,
            &TerminalEvent::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            area,
        );
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("line-099"));
        handle_input(
            &mut app,
            &wheel(MouseEventKind::Down(MouseButton::Left), 2),
            area,
        );
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn services_are_listed_vertically_on_the_left() {
        let mut app = App::terminal(
            &[
                Service {
                    name: "first".into(),
                    cmd: "true".into(),
                    cwd: ".".into(),
                    autostart: false,
                    hide_stderr: false,
                },
                Service {
                    name: "second".into(),
                    cmd: "true".into(),
                    cwd: ".".into(),
                    autostart: false,
                    hide_stderr: false,
                },
            ],
            TerminalSize::new(9, 52),
        );
        app.services[0]
            .terminal
            .as_mut()
            .unwrap()
            .process(b"LOG_CONTENT");
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let left: Vec<String> = (0..12)
            .map(|row| {
                (0..26)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect()
            })
            .collect();
        let first = left
            .iter()
            .position(|row| row.contains("first"))
            .expect("first service on left");
        let second = left
            .iter()
            .position(|row| row.contains("second"))
            .expect("second service on left");
        assert!(
            second > first,
            "service names must be on separate sidebar rows"
        );
        assert!(!left.iter().any(|row| row.contains("LOG_CONTENT")));
    }

    #[test]
    fn renders_selection_status_empty_output_and_small_sizes() {
        let mut app = App::terminal(
            &[
                Service {
                    name: "first".into(),
                    cmd: "true".into(),
                    cwd: ".".into(),
                    autostart: false,
                    hide_stderr: false,
                },
                Service {
                    name: "second".into(),
                    cmd: "true".into(),
                    cwd: ".".into(),
                    autostart: false,
                    hide_stderr: false,
                },
            ],
            TerminalSize::new(9, 52),
        );
        app.selected = 1;
        app.services[1].state = State::Exited(Some(42));
        for (width, height) in [(80, 24), (24, 6), (8, 2), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &app)).unwrap();
            if width == 80 {
                let contents = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>();
                assert!(contents.contains("second"));
                assert!(contents.contains("exited (42)"));
                assert!(contents.contains("No output yet"));
                assert!(contents.contains("first"));
            } else if width == 24 {
                let top = (0..width)
                    .map(|column| terminal.backend().buffer()[(column, 0)].symbol())
                    .collect::<String>();
                assert!(top.contains("second"));
            }
        }
    }
}
