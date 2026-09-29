//! Append-only failure history; occurrence counts are derived from persisted records.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct Record {
    time: String,
    session: String,
    kind: String,
    message: String,
    code: String,
}

pub struct FailureLog {
    path: PathBuf,
    counts: HashMap<(String, String), u64>,
    needs_separator: bool,
}

impl FailureLog {
    /// Restore counts without creating the log; malformed history stops startup.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("failures.jsonl");
        let mut log = Self {
            path,
            counts: HashMap::new(),
            needs_separator: false,
        };
        let file = match File::open(&log.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(log),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", log.path.display()));
            }
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut number = 0;
        loop {
            line.clear();
            number += 1;
            if reader
                .read_line(&mut line)
                .with_context(|| format!("reading {} line {number}", log.path.display()))?
                == 0
            {
                break;
            }
            let record: Record = serde_json::from_str(&line)
                .with_context(|| format!("parsing {} line {number}", log.path.display()))?;
            validate_kind(&record.kind)
                .with_context(|| format!("parsing {} line {number}", log.path.display()))?;
            chrono::DateTime::parse_from_rfc3339(&record.time).with_context(|| {
                format!("parsing {} line {number}: invalid time", log.path.display())
            })?;
            // A valid final record may omit its newline; separate the next
            // append without changing the existing record or writing at load.
            log.needs_separator = !line.ends_with('\n');
            let count = log
                .counts
                .entry(failure_key(&record.kind, &record.message))
                .or_insert(0);
            *count = count.checked_add(1).with_context(|| {
                format!("count overflow in {} line {number}", log.path.display())
            })?;
        }
        Ok(log)
    }

    /// Persist the complete record before advancing its displayed occurrence count.
    pub fn record(
        &mut self,
        session: &str,
        kind: &str,
        message: &str,
        code: &str,
    ) -> anyhow::Result<u64> {
        validate_kind(kind).with_context(|| format!("appending {}", self.path.display()))?;
        let key = failure_key(kind, message);
        let count = self
            .counts
            .get(&key)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .with_context(|| format!("count overflow in {}", self.path.display()))?;
        let record = Record {
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            session: session.to_owned(),
            kind: kind.to_owned(),
            message: message.to_owned(),
            code: code.to_owned(),
        };
        let mut bytes = serde_json::to_vec(&record)
            .with_context(|| format!("encoding record for {}", self.path.display()))?;
        if self.needs_separator {
            bytes.insert(0, b'\n');
        }
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {} for append", self.path.display()))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("appending {}", self.path.display()))?;
        self.needs_separator = false;
        self.counts.insert(key, count);
        Ok(count)
    }
}

/// Match the UI's first-line grouping while retaining the full message on disk.
fn failure_key(kind: &str, message: &str) -> (String, String) {
    (
        kind.to_owned(),
        message.lines().next().unwrap_or("").to_owned(),
    )
}

/// Reject unknown categories so malformed history cannot create misleading counts.
fn validate_kind(kind: &str) -> anyhow::Result<()> {
    if !matches!(kind, "type check" | "denied" | "unsupported") {
        bail!("unknown failure kind: {kind:?}");
    }
    Ok(())
}
