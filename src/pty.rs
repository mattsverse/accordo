//! Unix PTY transport. The terminal library, not Accordo, interprets output bytes.

use std::{
    io,
    os::fd::{BorrowedFd, OwnedFd},
    sync::Arc,
};

use nix::{
    errno::Errno,
    fcntl::{FcntlArg, OFlag, fcntl},
    unistd::{dup, read, write},
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::io::unix::AsyncFd;

use crate::config::Service;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl TerminalSize {
    #[must_use]
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: if rows == 0 { 1 } else { rows },
            cols: if cols == 0 { 1 } else { cols },
        }
    }

    fn pty(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

pub(crate) struct Session {
    pub child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    pub io: Arc<PtyIo>,
}

impl Session {
    pub fn spawn(service: &Service, size: TerminalSize) -> io::Result<Self> {
        let pair = native_pty_system()
            .openpty(size.pty())
            .map_err(io::Error::other)?;
        let raw = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("PTY has no Unix descriptor"))?;
        // SAFETY: the owning master remains alive while dup creates an
        // independently owned descriptor. No borrowed descriptor escapes.
        let fd = dup(unsafe { BorrowedFd::borrow_raw(raw) }).map_err(io::Error::from)?;
        let flags =
            OFlag::from_bits_truncate(fcntl(&fd, FcntlArg::F_GETFL).map_err(io::Error::from)?);
        fcntl(&fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::from)?;
        let io = Arc::new(PtyIo(AsyncFd::new(fd)?));
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        // Pass the command as a separate argument, preserving its quoting and
        // ensuring every command in the script inherits the stderr redirect.
        if service.hide_stderr {
            command.arg("exec 2>/dev/null; exec /bin/sh -c \"$1\"");
            command.arg("accordo");
        }
        command.arg(&service.cmd);
        command.cwd(&service.cwd);
        command.env("TERM", "xterm-256color");
        command.env_remove("COLUMNS");
        command.env_remove("LINES");
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;
        // Keeping a slave descriptor in Accordo would prevent EOF after child exit.
        drop(pair.slave);
        Ok(Self {
            child,
            master: pair.master,
            io,
        })
    }

    pub fn resize(&self, size: TerminalSize) -> io::Result<()> {
        self.master.resize(size.pty()).map_err(io::Error::other)
    }
}

pub(crate) struct PtyIo(AsyncFd<OwnedFd>);

impl PtyIo {
    pub async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.0.readable().await?;
            if let Ok(result) = ready.try_io(|fd| match read(fd.get_ref(), buffer) {
                // Linux reports EIO when the last slave closes; macOS uses EOF.
                Err(Errno::EIO) => Ok(0),
                result => result.map_err(io::Error::from),
            }) {
                return result;
            }
        }
    }

    pub async fn reply(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let mut ready = self.0.writable().await?;
            if let Ok(result) =
                ready.try_io(|fd| write(fd.get_ref(), bytes).map_err(io::Error::from))
            {
                let count = result?;
                if count == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                bytes = &bytes[count..];
            }
        }
        Ok(())
    }
}
