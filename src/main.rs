mod app;
mod client;
mod config;
mod input;
mod repl;
mod session;
mod tool_reasoning;
mod transcript;
mod tui;

use std::path::PathBuf;

use clap::Parser;

use session::{NOT_RUN, Session};

#[derive(Parser)]
struct Args {
    /// Continue an existing session file, appending to it.
    #[arg(long, value_name = "PATH")]
    resume: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    // Config and session errors are reported before the terminal UI starts.
    let settings = config::load()?;
    let session = match args.resume {
        Some(path) => {
            let (session, repaired) = Session::resume(path)?;
            // Printed to the normal screen before the TUI's alternate screen, so it's
            // there after exit.
            if repaired > 0 {
                println!(
                    "{}: added {NOT_RUN} for {repaired} tool calls without results",
                    session.path.display()
                );
            }
            session
        }
        None => Session::new(),
    };
    app::run(settings, session).await
}
