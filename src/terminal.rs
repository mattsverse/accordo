//! Terminal state and scrollback delegated to vt100, rendered by tui-term.

use tui_term::vt100::{self, Callbacks};

use crate::pty::TerminalSize;

pub const HISTORY_LIMIT: usize = 10_000;

#[derive(Default)]
struct Replies(Vec<u8>);

impl Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        second: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        if intermediate.is_some() || second.is_some() || params.len() > 1 {
            return;
        }
        let parameter = params
            .first()
            .and_then(|values| values.first())
            .copied()
            .unwrap_or(0);
        let reply = match (command, parameter) {
            ('n', 5) => "\x1b[0n".into(),
            ('n', 6) => {
                let (row, col) = screen.cursor_position();
                format!("\x1b[{};{}R", row + 1, col + 1)
            }
            ('c', 0) => "\x1b[?1;2c".into(),
            ('t', 18) => {
                let (rows, cols) = screen.size();
                format!("\x1b[8;{rows};{cols}t")
            }
            _ => return,
        };
        if self.0.len() + reply.len() <= 32 * 1024 {
            self.0.extend_from_slice(reply.as_bytes());
        }
    }
}

pub struct TerminalView {
    parser: vt100::Parser<Replies>,
    pub received_output: bool,
}

impl TerminalView {
    #[must_use]
    pub fn new(size: TerminalSize) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                size.rows,
                size.cols,
                HISTORY_LIMIT,
                Replies::default(),
            ),
            received_output: false,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.received_output |= !bytes.is_empty();
        self.parser.process(bytes);
    }

    pub fn start_run(&mut self, run: u64) {
        if run <= 1 {
            return;
        }
        let previous = self.parser.screen().clone();
        let (rows, cols) = previous.size();
        // A new parser drops incomplete escape sequences from the old process.
        self.parser =
            vt100::Parser::new_with_callbacks(rows, cols, HISTORY_LIMIT, Replies::default());
        *self.parser.screen_mut() = previous;
        self.parser.process(
            format!(
                "\x1b[?1049l\x1b[0m\x1b[r\x1b[?7h\x1b[?25h\x1b[{rows};1H\r\n--- run {run} ---\r\n"
            )
            .as_bytes(),
        );
    }

    pub fn resize(&mut self, size: TerminalSize) {
        if self.parser.screen().size() != (size.rows, size.cols) {
            self.parser.screen_mut().set_size(size.rows, size.cols);
        }
    }

    pub fn scroll(&mut self, up: bool, rows: usize) {
        let offset = self.offset();
        self.parser.screen_mut().set_scrollback(if up {
            offset.saturating_add(rows)
        } else {
            offset.saturating_sub(rows)
        });
    }

    #[must_use]
    pub fn offset(&self) -> usize {
        self.parser.screen().scrollback()
    }

    #[must_use]
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.parser.callbacks_mut().0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_colors_redraws_partial_sequences_and_alternate_screens() {
        let mut terminal = TerminalView::new(TerminalSize::new(4, 20));
        terminal.process(b"old progress\r\x1b[2K\x1b[3");
        terminal.process(b"1mready\x1b[0m");
        assert_eq!(terminal.screen().contents(), "ready");
        assert_eq!(
            terminal.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
        terminal.process(b"\x1b[?1049h\x1b[2J\x1b[Hother");
        assert_eq!(terminal.screen().contents(), "other");
        terminal.process(b"\x1b[?1049l");
        assert_eq!(terminal.screen().contents(), "ready");
    }

    #[test]
    fn scrollback_stays_anchored_and_is_bounded() {
        let mut terminal = TerminalView::new(TerminalSize::new(3, 20));
        for index in 0..20 {
            terminal.process(format!("line-{index}\r\n").as_bytes());
        }
        terminal.scroll(true, 4);
        let before = terminal.screen().contents();
        terminal.process(b"another\r\n");
        assert_eq!(terminal.screen().contents(), before);
        assert_eq!(terminal.offset(), 5);
        terminal.scroll(false, usize::MAX);
        assert!(terminal.screen().contents().contains("another"));
        for _ in 0..HISTORY_LIMIT + 10 {
            terminal.process(b"line\r\n");
        }
        terminal.scroll(true, usize::MAX);
        assert_eq!(terminal.offset(), HISTORY_LIMIT);
        terminal.resize(TerminalSize::new(10, 40));
        assert_eq!(terminal.screen().size(), (10, 40));
    }

    #[test]
    fn restarts_drop_partial_sequences_and_answer_terminal_queries() {
        let mut terminal = TerminalView::new(TerminalSize::new(4, 20));
        terminal.process(b"previous\x1b[");
        terminal.start_run(2);
        terminal.process(b"new\x1b[6n\x1b[5n");
        assert!(terminal.screen().contents().contains("--- run 2 ---"));
        assert!(terminal.screen().contents().contains("new"));
        assert!(terminal.take_replies().ends_with(b"R\x1b[0n"));
        terminal.scroll(true, usize::MAX);
        assert!(terminal.screen().contents().contains("previous"));
    }
}
