mod app;
mod client;
mod commands;
mod config;
mod input;
mod repl;
mod session;
mod tool_reasoning;
mod transcript;
mod tui;

use clap::Parser;

use session::{NOT_RUN, Session};

#[derive(Parser)]
struct Args {
    /// Continue the named session, appending to it. Without a name, list the saved
    /// sessions and exit.
    #[arg(long, value_name = "NAME")]
    resume: Option<Option<String>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let dir = config::harness_dir()?;
    // Listing needs no config, so it works even while the config is broken.
    if let Some(None) = args.resume {
        for name in session::list(&dir)? {
            println!("{name}");
        }
        return Ok(());
    }
    // Config and session errors are reported before the terminal UI starts.
    let settings = config::load(&dir)?;
    let session = match args.resume {
        Some(Some(name)) => {
            let (session, repaired) = Session::resume(&dir, &name)?;
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
        _ => Session::new(&dir),
    };
    app::run(settings, session).await
}
