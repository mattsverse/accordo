use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{
    config::Service,
    pty::TerminalSize,
    supervisor::{Command, Event, EventKind, State, Stream},
    terminal::TerminalView,
};

pub struct ServiceView {
    pub name: String,
    pub autostart: bool,
    pub hide_stderr: bool,
    pub state: State,
    pub run: u64,
    pub terminal: Option<TerminalView>,
}

pub struct App {
    pub services: Vec<ServiceView>,
    pub selected: usize,
    pub failed: bool,
    log_height: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Quit(u8),
    Service(usize, Command),
}

impl App {
    #[must_use]
    pub fn new(services: &[Service]) -> Self {
        Self {
            services: services
                .iter()
                .map(|service| ServiceView {
                    name: service.name.clone(),
                    autostart: service.autostart,
                    hide_stderr: service.hide_stderr,
                    state: State::Stopped,
                    run: 0,
                    terminal: None,
                })
                .collect(),
            selected: 0,
            failed: false,
            log_height: 1,
        }
    }

    #[must_use]
    pub fn terminal(services: &[Service], size: TerminalSize) -> Self {
        let mut app = Self::new(services);
        for service in &mut app.services {
            service.terminal = Some(TerminalView::new(size));
        }
        app.log_height = usize::from(size.rows);
        app
    }

    /// Apply output to its terminal, or return a printable line for linear mode.
    pub fn apply(&mut self, event: Event) -> Option<String> {
        let service = self.services.get_mut(event.service)?;
        if event.run < service.run {
            return None;
        }
        let new_run = event.run > service.run;
        service.run = event.run;
        let text = match event.kind {
            EventKind::Terminal { bytes, .. } => {
                if let Some(terminal) = &mut service.terminal {
                    terminal.process(&bytes);
                }
                return None;
            }
            EventKind::State(state) => {
                if new_run
                    && state == State::Starting
                    && let Some(terminal) = &mut service.terminal
                {
                    terminal.start_run(event.run);
                }
                self.failed |= matches!(&state, State::Failed(_) | State::Exited(None))
                    || matches!(&state, State::Exited(Some(code)) if *code != 0);
                let text = if state == State::Starting {
                    format!("--- run {} ---", service.run)
                } else {
                    format!("--- {} ---", state.label())
                };
                service.state = state;
                text
            }
            EventKind::Output { stream, text, .. } => match stream {
                Stream::Stdout => text,
                Stream::Stderr if service.hide_stderr => return None,
                Stream::Stderr => format!("[stderr] {text}"),
            },
        };
        Some(format!("[{} {}] {text}", event.service, service.name))
    }

    #[must_use]
    pub fn all_enabled_finished(&self) -> bool {
        self.services
            .iter()
            .filter(|service| service.autostart)
            .all(|service| service.run > 0 && service.state.is_finished())
    }

    pub fn resize_terminal(&mut self, size: TerminalSize) {
        self.log_height = usize::from(size.rows);
        for service in &mut self.services {
            if let Some(terminal) = &mut service.terminal {
                terminal.resize(size);
            }
        }
    }

    pub fn scroll_logs(&mut self, up: bool, lines: usize) {
        if let Some(service) = self.services.get_mut(self.selected)
            && let Some(terminal) = &mut service.terminal
        {
            terminal.scroll(up, lines);
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Action::Quit(130));
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::PageUp => self.scroll_logs(true, self.log_height.saturating_sub(1).max(1)),
            KeyCode::PageDown => self.scroll_logs(false, self.log_height.saturating_sub(1).max(1)),
            KeyCode::Home => self.scroll_logs(true, usize::MAX),
            KeyCode::End => self.scroll_logs(false, usize::MAX),
            KeyCode::Char('q') => return Some(Action::Quit(u8::from(self.failed))),
            KeyCode::Up if !self.services.is_empty() => {
                self.selected = (self.selected + self.services.len() - 1) % self.services.len();
            }
            KeyCode::Down if !self.services.is_empty() => {
                self.selected = (self.selected + 1) % self.services.len();
            }
            KeyCode::Char(digit @ '0'..='9') => {
                let index = digit as usize - '0' as usize;
                if index < self.services.len() {
                    self.selected = index;
                }
            }
            KeyCode::Char('s' | 'r') => {
                let service = self.services.get_mut(self.selected)?;
                if matches!(service.state, State::Starting | State::Stopping) {
                    return None;
                }
                let command = match key.code {
                    KeyCode::Char('r') => Command::Restart,
                    _ if service.state.is_active() => Command::Stop,
                    _ => Command::Start,
                };
                service.state = if service.state.is_active() {
                    State::Stopping
                } else {
                    State::Starting
                };
                return Some(Action::Service(self.selected, command));
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(
            &(0..12)
                .map(|index| Service {
                    name: format!("app{index}"),
                    cmd: "true".into(),
                    cwd: ".".into(),
                    autostart: true,
                    hide_stderr: false,
                })
                .collect::<Vec<_>>(),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn selection_wraps_and_lifecycle_keys_coalesce() {
        let mut app = app();
        app.key(key(KeyCode::Up));
        assert_eq!(app.selected, 11);
        app.key(key(KeyCode::Down));
        assert_eq!(app.selected, 0);
        app.key(key(KeyCode::Char('9')));
        assert_eq!(app.selected, 9);
        assert_eq!(
            app.key(key(KeyCode::Char('s'))),
            Some(Action::Service(9, Command::Start))
        );
        assert_eq!(app.key(key(KeyCode::Char('r'))), None);
        app.services[9].state = State::Running;
        assert_eq!(
            app.key(key(KeyCode::Char('r'))),
            Some(Action::Service(9, Command::Restart))
        );
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit(130))
        );
    }

    #[test]
    fn linear_stderr_filter_keeps_stdout_and_failures() {
        let capacity = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let mut app = app();
        app.services[0].hide_stderr = true;
        let output = |service, stream, text: &str| Event {
            service,
            run: 1,
            kind: EventKind::Output {
                stream,
                text: text.into(),
                permit: capacity.clone().try_acquire_owned().unwrap(),
            },
        };
        assert!(app.apply(output(0, Stream::Stderr, "hidden")).is_none());
        assert_eq!(capacity.available_permits(), 1);
        assert_eq!(
            app.apply(output(0, Stream::Stdout, "stdout")).unwrap(),
            "[0 app0] stdout"
        );
        assert_eq!(
            app.apply(output(1, Stream::Stderr, "visible")).unwrap(),
            "[1 app1] [stderr] visible"
        );
        assert!(
            app.apply(Event {
                service: 0,
                run: 1,
                kind: EventKind::State(State::Exited(Some(7)))
            })
            .unwrap()
            .contains("exited (7)")
        );
        assert!(app.failed);
    }

    #[test]
    fn terminal_history_is_per_service_and_stale_runs_are_ignored() {
        let mut app = app();
        for service in &mut app.services {
            service.terminal = Some(TerminalView::new(TerminalSize::new(4, 20)));
        }
        app.resize_terminal(TerminalSize::new(4, 20));
        app.services[0].run = 2;
        for index in 0..20 {
            app.services[0]
                .terminal
                .as_mut()
                .unwrap()
                .process(format!("line-{index}\r\n").as_bytes());
        }
        app.key(key(KeyCode::PageUp));
        assert_eq!(app.services[0].terminal.as_ref().unwrap().offset(), 3);
        app.key(key(KeyCode::Char('1')));
        assert_eq!(app.services[1].terminal.as_ref().unwrap().offset(), 0);
        app.key(key(KeyCode::Char('0')));
        assert_eq!(app.services[0].terminal.as_ref().unwrap().offset(), 3);
        let before = app.services[0]
            .terminal
            .as_ref()
            .unwrap()
            .screen()
            .contents();
        let permit = std::sync::Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap();
        app.apply(Event {
            service: 0,
            run: 1,
            kind: EventKind::Terminal {
                bytes: b"stale".to_vec(),
                permit,
            },
        });
        assert_eq!(
            app.services[0]
                .terminal
                .as_ref()
                .unwrap()
                .screen()
                .contents(),
            before
        );
        assert!(
            app.apply(Event {
                service: 0,
                run: 1,
                kind: EventKind::State(State::Exited(Some(1)))
            })
            .is_none()
        );
        assert!(!app.failed);
        app.key(key(KeyCode::End));
        assert_eq!(app.services[0].terminal.as_ref().unwrap().offset(), 0);
    }
}
