//! `workspace/<name>`: a directory and a shell, as a Restate virtual object,
//! for coding agents to work in. Each key is a directory under the root
//! (`FUNCTIONS_WORKSPACES`, by default `workspaces/`), made on first use:
//!
//! | Handler | Input                       | Output                          |
//! | ------- | --------------------------- | ------------------------------- |
//! | `bash`  | `{command, timeout?}` (s)   | `{exit, output}`: stdout+stderr |
//! | `read`  | `{path}`                    | The file's text                 |
//! | `write` | `{path, content}`           | What it did                     |
//! | `edit`  | `{path, old, new}`          | What it did; `old` must be unique |
//!
//! Paths are relative to the workspace. A failure the caller can act on (no
//! such file, `old` not found) fails the call with a terminal error; a
//! command that fails is an `exit` code, not an error.
//!
//! Changes run one at a time per workspace (`read` alongside them). Each is
//! journaled once done (`ctx.run`), so a retried call does not run a command
//! again; one cut off by a crash runs again.
//!
//! Commands get the workspace as working and home directory, and an
//! environment of `PATH` alone: the server's secrets (`ANTHROPIC_API_KEY`)
//! are not theirs. This is no sandbox: serve it where a model may run
//! anything.
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

/// Output kept of a command, or of a file read: its head and its tail.
const MAX_OUTPUT: usize = 30_000;
const MAX_READ: usize = 100_000;
const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 600;

pub struct Workspace {
    root: PathBuf,
}

#[derive(Deserialize)]
pub struct Bash {
    command: String,
    /// Seconds before the command is killed.
    timeout: Option<u64>,
}

#[derive(Serialize, Deserialize)]
pub struct Ran {
    exit: i32,
    output: String,
}

#[derive(Deserialize)]
pub struct Read {
    path: String,
}

#[derive(Deserialize)]
pub struct Write {
    path: String,
    content: String,
}

#[derive(Deserialize)]
pub struct Edit {
    path: String,
    old: String,
    new: String,
}

impl Workspace {
    /// Workspaces under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Workspaces under `FUNCTIONS_WORKSPACES`, by default `workspaces/`.
    pub fn from_env() -> Self {
        Self::new(std::env::var_os("FUNCTIONS_WORKSPACES").unwrap_or_else(|| "workspaces".into()))
    }

    /// The workspace's directory, made if new.
    async fn dir(&self, key: &str) -> Result<PathBuf, HandlerError> {
        if key.is_empty() || key.len() > 255 || key.starts_with('.') || key.contains(['/', '\\']) {
            return Err(TerminalError::new(format!("Invalid workspace name {key:?}")).into());
        }
        let dir = self.root.join(key);
        tokio::fs::create_dir_all(&dir).await?;
        Ok(dir)
    }

    /// `path` in the workspace `key`: relative, and never out of it.
    async fn path(&self, key: &str, path: &str) -> Result<PathBuf, HandlerError> {
        let relative = Path::new(path);
        let inside = relative
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if path.is_empty() || !inside {
            let error = format!("{path:?}: use a path relative to the workspace, without '..'");
            return Err(TerminalError::new(error).into());
        }
        Ok(self.dir(key).await?.join(relative))
    }
}

#[restate_sdk::object(name = "workspace")]
impl Workspace {
    /// Run `command` with bash in the workspace.
    #[handler]
    async fn bash(&self, ctx: ObjectContext<'_>, input: Json<Bash>) -> HandlerResult<Json<Ran>> {
        let Bash { command, timeout } = input.into_inner();
        let dir = self.dir(ctx.key()).await?;
        let timeout = Duration::from_secs(timeout.unwrap_or(DEFAULT_TIMEOUT).min(MAX_TIMEOUT));
        ctx.run(|| run(&dir, &command, timeout))
            .name("bash")
            .await
            .map_err(Into::into)
    }

    /// The text of the file at `path`.
    #[handler]
    async fn read(&self, ctx: SharedObjectContext<'_>, input: Json<Read>) -> HandlerResult<String> {
        let Read { path } = input.into_inner();
        let file = self.path(ctx.key(), &path).await?;
        let bytes = tokio::fs::read(&file).await.map_err(|e| failed(&path, e))?;
        Ok(clip(&String::from_utf8_lossy(&bytes), MAX_READ))
    }

    /// Write `content` to the file at `path`, and the directories to it.
    #[handler]
    async fn write(&self, ctx: ObjectContext<'_>, input: Json<Write>) -> HandlerResult<String> {
        let Write { path, content } = input.into_inner();
        let file = self.path(ctx.key(), &path).await?;
        ctx.run(|| async {
            if let Some(parent) = file.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&file, &content)
                .await
                .map_err(|e| failed(&path, e))?;
            Ok(format!("Wrote {} bytes to {path}", content.len()))
        })
        .name("write")
        .await
        .map_err(Into::into)
    }

    /// Replace `old`, which must occur exactly once in the file, with `new`.
    #[handler]
    async fn edit(&self, ctx: ObjectContext<'_>, input: Json<Edit>) -> HandlerResult<String> {
        let Edit { path, old, new } = input.into_inner();
        let file = self.path(ctx.key(), &path).await?;
        ctx.run(|| async {
            let text = tokio::fs::read_to_string(&file)
                .await
                .map_err(|e| failed(&path, e))?;
            match text.matches(old.as_str()).count() {
                1 => {}
                0 => return Err(TerminalError::new(format!("{path}: text not found")).into()),
                n => {
                    let error = format!("{path}: text found {n} times; include more around it");
                    return Err(TerminalError::new(error).into());
                }
            }
            tokio::fs::write(&file, text.replacen(old.as_str(), &new, 1)).await?;
            Ok(format!("Edited {path}"))
        })
        .name("edit")
        .await
        .map_err(Into::into)
    }
}

/// Run `command` in `dir` until it exits or `timeout` passes, and then kill
/// it with everything it started.
async fn run(dir: &Path, command: &str, timeout: Duration) -> HandlerResult<Json<Ran>> {
    let (exit, output) = run_command(dir, command, timeout).await?;
    Ok(Json(Ran { exit, output }))
}

/// Run `command` with bash in `dir`, with a clean environment, until it
/// exits or `timeout` passes, and then kill it with everything it started:
/// its exit code (-1 if killed), and its output, stdout then stderr, cut to
/// 30 KB.
pub(crate) async fn run_command(
    dir: &Path,
    command: &str,
    timeout: Duration,
) -> std::io::Result<(i32, String)> {
    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let group = child.id().map(|pid| pid as i32);
    let ran = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(output) => {
            let output = output?;
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            (output.status.code().unwrap_or(-1), clip(&text, MAX_OUTPUT))
        }
        Err(_) => (-1, format!("Killed after {} s", timeout.as_secs())),
    };
    if let Some(group) = group {
        // What the command left running, in its process group.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    }
    Ok(ran)
}

/// `text`, its middle cut to keep it within `max` bytes.
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let floor = |mut at: usize| {
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let head = floor(max / 2);
    let tail = floor(text.len() - max / 2);
    let cut = tail - head;
    format!(
        "{}\n[... {cut} bytes cut ...]\n{}",
        &text[..head],
        &text[tail..]
    )
}

/// A file error the caller can act on, as a terminal error.
fn failed(path: &str, error: std::io::Error) -> HandlerError {
    TerminalError::new(format!("{path}: {error}")).into()
}
