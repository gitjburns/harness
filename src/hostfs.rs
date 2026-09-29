//! Permission checks before Monty services repository filesystem operations.

use std::path::Path;

use monty_pool::ResumeValue;
use monty_types::{
    CallArgs, ExcType, FileMode, MontyException, MontyObject,
    unstable::{self, MontyNode},
};

use crate::{config::Permission, host::HostContext};

/// Filesystem execution belongs exclusively to the repository mount.
pub enum FsAction {
    Mounts,
    Reply(ResumeValue),
}

/// Check each operation against the current permission without retaining grants.
pub fn handle(name: &str, args: &CallArgs, host: &HostContext) -> FsAction {
    match dispatch(name, args, host) {
        Ok(action) => action,
        Err(error) => FsAction::Reply(ResumeValue::Error(error)),
    }
}

/// Keep the environment empty and send recognized filesystem calls through mounts.
fn dispatch(name: &str, args: &CallArgs, host: &HostContext) -> Result<FsAction, MontyException> {
    match name {
        "os.getenv" => {
            return Ok(FsAction::Reply(ResumeValue::Return(
                args.arg(1).map_or_else(MontyObject::none, |v| v.to_owned()),
            )));
        }
        "os.environ" => {
            return Ok(FsAction::Reply(ResumeValue::Return(MontyObject::dict([]))));
        }
        _ => {}
    }
    let write = match name {
        "Path.write_text" | "Path.write_bytes" | "Path.append_text" | "Path.append_bytes"
        | "Path.mkdir" | "Path.unlink" | "Path.rmdir" | "Path.rename" => true,
        "open" => open_mode(args)?.create(),
        "Path.exists" | "Path.is_file" | "Path.is_dir" | "Path.is_symlink" | "Path.read_text"
        | "Path.read_bytes" | "Path.stat" | "Path.iterdir" | "Path.resolve" | "Path.absolute" => {
            false
        }
        _ => return Ok(FsAction::Reply(ResumeValue::NotHandled)),
    };
    authorize(path_arg(args, 0)?, write, host)?;
    if name == "Path.rename" {
        // Both endpoints must be covered before the mount can perform a rename.
        authorize(path_arg(args, 1)?, true, host)?;
    }
    Ok(FsAction::Mounts)
}

/// Reject uncovered paths and disallowed operations without performing host I/O.
fn authorize(path: &Path, write: bool, host: &HostContext) -> Result<(), MontyException> {
    let kind = if write { "write" } else { "read" };
    // CallArgs carries Monty's normalized spelling. This prefix check only
    // rejects inputs outside mount coverage; the mount checks the original call
    // and is authoritative for symlink and traversal confinement.
    if !path.starts_with(&host.repo) {
        return Err(host.denied(format!(
            "{kind} {}: denied: the path is outside the repository",
            path.display()
        )));
    }
    let permission = host.permission();
    if permission == Permission::None || (write && permission == Permission::ReadOnly) {
        return Err(host.denied(format!(
            "{kind} {}: denied: the permission level is {}",
            path.display(),
            permission.as_str()
        )));
    }
    Ok(())
}

/// Extract Monty's normalized path node without accepting arbitrary Python values.
fn path_arg(args: &CallArgs, index: usize) -> Result<&Path, MontyException> {
    match args.arg(index).map(unstable::node) {
        Some(MontyNode::Path(path)) => Ok(Path::new(path)),
        _ => Err(MontyException::new(
            ExcType::TypeError,
            Some(format!("filesystem argument {index}: expected a path")),
        )),
    }
}

/// Classify open-time effects from the mode Monty supplies as a string.
fn open_mode(args: &CallArgs) -> Result<FileMode, MontyException> {
    let mode = args
        .arg(1)
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            MontyException::new(
                ExcType::TypeError,
                Some("open: expected a mode string".into()),
            )
        })?;
    mode.parse()
        .map_err(|error: std::borrow::Cow<'static, str>| {
            MontyException::new(ExcType::ValueError, Some(format!("open: {error}")))
        })
}
