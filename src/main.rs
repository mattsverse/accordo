use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
};

use accordo::{
    app::App,
    config::{Config, Layout},
    supervisor::Supervisor,
    view::{self, Signals, TerminalGuard},
};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "accordo",
    version,
    about = "Run services in terminal tabs or a merged log stream"
)]
struct Args {
    /// Configuration file; relative service directories resolve beside this file.
    #[arg(short = 'f', long = "file", default_value = "accordo.yaml")]
    file: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("accordo: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<u8, String> {
    let config = Config::load(&args.file)?;
    if config.layout == Layout::Tabbed && !(io::stdin().is_terminal() && io::stdout().is_terminal())
    {
        return Err("tabbed layout requires an interactive terminal; set layout: linear in your configuration for piped output".into());
    }
    let mut signals = Signals::new().map_err(|error| error.to_string())?;
    let mut terminal = if config.layout == Layout::Tabbed {
        Some(TerminalGuard::new().map_err(|error| error.to_string())?)
    } else {
        None
    };
    let (mut app, mut supervisor) = if let Some(guard) = &mut terminal {
        let size = guard.terminal.size().map_err(|error| error.to_string())?;
        let size = view::terminal_size(ratatui::layout::Rect::new(0, 0, size.width, size.height));
        (
            App::terminal(&config.services, size),
            Supervisor::terminal(&config.services, size),
        )
    } else {
        (
            App::new(&config.services),
            Supervisor::new(&config.services),
        )
    };
    let result = match terminal.as_mut() {
        Some(guard) => {
            view::tabbed(&mut app, &mut supervisor, &mut signals, &mut guard.terminal).await
        }
        None => view::linear(&mut app, &mut supervisor, &mut signals).await,
    };
    // Restore the terminal immediately; process cleanup can take three seconds.
    drop(terminal);
    let cleanup = supervisor.shutdown().await;
    let code = result.map_err(|error| error.to_string())?;
    cleanup?;
    Ok(code)
}
