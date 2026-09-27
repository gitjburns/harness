//! The REPL process: a Python driver (`repl_driver.py`, embedded) run locally and
//! unsandboxed in the working directory. One process lives for one turn.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const DRIVER: &str = include_str!("repl_driver.py");

/// Function library directory, inside the harness directory.
const LIBRARY_DIR: &str = "replib";

pub enum ReplEvent {
    /// Output as it is written, in order, from the code and anything it started.
    Output(String),
    /// The call finished. `value` is the repr of a trailing expression, if not None.
    Done {
        value: Option<String>,
        error: Option<String>,
    },
}

#[derive(Deserialize)]
struct DriverMessage {
    output: Option<String>,
    #[serde(default)]
    done: bool,
    value: Option<String>,
    error: Option<String>,
}

pub struct Repl {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    /// The driver calls `setsid()` first thing, becoming the leader of a new session
    /// and process group whose id is its pid. Killing the group also kills any
    /// programs the code started.
    pgid: u32,
}

impl Repl {
    /// `harness_dir` holds `replib/`; the process itself runs in the working directory.
    pub fn start(output_limit: usize, model: &str, harness_dir: &Path) -> anyhow::Result<Repl> {
        let mut child = Command::new("python3")
            .arg("-u")
            .arg("-c")
            .arg(DRIVER)
            .env("HARNESS_OUTPUT_LIMIT", output_limit.to_string())
            .env("HARNESS_MODEL", model)
            .env("HARNESS_LIBRARY_DIR", harness_dir.join(LIBRARY_DIR))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The driver redirects fd 2 into its capture pipe at startup; anything
            // written before that must not reach the terminal UI.
            .stderr(Stdio::null())
            // No `process_group(0)`: `setsid()` in the driver fails for a process that
            // already leads a group.
            .kill_on_drop(true)
            .spawn()
            .context("starting python3")?;
        let pgid = child.id().context("python3 exited immediately")?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        Ok(Repl {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            pgid,
        })
    }

    /// Run `code`. `prompt_tokens` is the latest reported prompt size, sent with every
    /// call so `FYI()` reports the count current at call time.
    pub async fn send(&mut self, code: &str, prompt_tokens: Option<u64>) -> anyhow::Result<()> {
        let line = format!(
            "{}\n",
            json!({ "code": code, "prompt_tokens": prompt_tokens })
        );
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("sending code to the REPL")
    }

    /// The next event of the call in progress. An error means the process died.
    pub async fn next_event(&mut self) -> anyhow::Result<ReplEvent> {
        let Some(line) = self
            .lines
            .next_line()
            .await
            .context("reading from the REPL")?
        else {
            bail!("the REPL process exited");
        };
        let message: DriverMessage =
            serde_json::from_str(&line).with_context(|| format!("bad REPL message: {line}"))?;
        Ok(if message.done {
            ReplEvent::Done {
                value: message.value,
                error: message.error,
            }
        } else {
            ReplEvent::Output(message.output.unwrap_or_default())
        })
    }

    /// Kill the driver and everything in its process group.
    pub fn kill(&mut self) {
        // `/bin/kill` accepts a negative pid as a process group on macOS and Linux,
        // which avoids a direct libc dependency.
        let _ = std::process::Command::new("kill")
            .args(["-s", "KILL", "--", &format!("-{}", self.pgid)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.start_kill();
    }
}

impl Drop for Repl {
    fn drop(&mut self) {
        self.kill();
    }
}
