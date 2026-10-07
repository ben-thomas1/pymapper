//! `pymapper-gui`: native code map. See the crate docs in lib.rs for the architecture.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use winit::event_loop::{ControlFlow, EventLoop};

use pymapper_gui::app::{App, UserEvent};
use pymapper_gui::args::{self, Options, Parsed, USAGE};

fn main() -> ExitCode {
    let launched = Instant::now();
    let opts = match args::parse(std::env::args_os().skip(1)) {
        Ok(Parsed::Run(opts)) => opts,
        Ok(Parsed::Help) => {
            let _ = std::io::stdout().lock().write_all(USAGE.as_bytes());
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            let _ = writeln!(std::io::stdout().lock(), "pymapper-gui {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            let _ = writeln!(std::io::stderr().lock(), "error: {msg}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(opts, launched) {
        Ok(Some(report)) => {
            // Graceful on a closed stdout.
            let _ = std::io::stdout().lock().write_all(report.as_bytes());
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr().lock(), "pymapper-gui: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(opts: Options, launched: Instant) -> Result<Option<String>> {
    let root = std::fs::canonicalize(&opts.path).with_context(|| format!("cannot open {}", opts.path.display()))?;
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let project = root.display().to_string();
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().context("creating event loop")?;
    // Sleep until events arrive; App switches to WaitUntil only when a repaint is scheduled.
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(opts, project, launched, event_loop.create_proxy());
    event_loop.run_app(&mut app).context("event loop")?;
    app.finish()
}
