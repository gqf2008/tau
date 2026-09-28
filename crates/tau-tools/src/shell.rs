//! `bash` and `powershell` — pi's `core/tools/bash.ts` and `powershell.ts`.
//!
//! The two share everything but discovery and a command prefix, exactly as pi
//! shares `createLocalShellOperations` between them: the command runs through
//! the platform's shell with stdout and stderr merged in arrival order, the
//! output is tail-truncated to pi's limits with the full stream spilled to a
//! temp file when it does not fit, and a `timeout:` kills the *process tree* —
//! a command that leaves a server behind must not outlive its timeout just
//! because the shell that started it is gone.
//!
//! These are host tools: they run with the permissions of the tau process, and
//! nothing asks the user first outside ACP mode. See `docs/builtin-tools.md`.

use std::env;
#[cfg(windows)]
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value as Json;
use tau_core::tool::{Tool, ToolDef, ToolOutput};

use crate::accumulate::{Accumulator, Snapshot};
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size};

/// pi's timeout ceiling, in milliseconds — a 32-bit signed millisecond count,
/// which is what Node's timer accepts.
const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;

/// The ceiling as the error message quotes it.
const MAX_TIMEOUT_SECONDS: f64 = MAX_TIMEOUT_MS / 1000.0;

/// How often the child is re-checked while its output streams in.
const POLL: Duration = Duration::from_millis(10);

/// How long output may keep arriving *after* the shell exited — a descendant
/// that inherited the pipes is still writing — before the tool calls the
/// command finished (pi's `EXIT_STDIO_GRACE_MS`).
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);

/// How long to wait for a killed process to actually die before giving up on
/// reaping it. The timeout message does not depend on the exit status.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// PowerShell's console encoding is not UTF-8 by default, so pi prefixes
/// every command with a line that switches it on. So does this.
pub const POWERSHELL_UTF8_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

/// How to start a shell: the program, its fixed arguments, and whether the
/// command travels on the command line or on stdin (the WSL launcher and
/// `bash -s` read it from stdin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    /// The executable.
    pub program: PathBuf,
    /// Arguments before the command.
    pub args: Vec<String>,
    /// True when the command goes to the child's stdin instead of argv.
    pub stdin_transport: bool,
}

/// Which of the two shells a [`ShellTool`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// The `bash` tool (Git Bash on Windows, `/bin/bash` or `sh` elsewhere).
    Bash,
    /// The `powershell` tool (Windows only).
    PowerShell,
}

/// The strings that differ between the two shell tools.
#[derive(Debug, Clone, Copy)]
struct ShellConfig {
    name: &'static str,
    shell_name: &'static str,
    temp_prefix: &'static str,
    command_prefix: &'static str,
}

const BASH: ShellConfig = ShellConfig {
    name: "bash",
    shell_name: "bash",
    temp_prefix: "tau-bash",
    command_prefix: "",
};

const POWERSHELL: ShellConfig = ShellConfig {
    name: "powershell",
    shell_name: "PowerShell",
    temp_prefix: "tau-powershell",
    command_prefix: POWERSHELL_UTF8_PREFIX,
};

impl ShellKind {
    fn config(self) -> ShellConfig {
        match self {
            ShellKind::Bash => BASH,
            ShellKind::PowerShell => POWERSHELL,
        }
    }

    /// Find the shell to run, or say why there is none.
    fn discover(self) -> Result<Shell, String> {
        match self {
            ShellKind::Bash => find_bash(),
            ShellKind::PowerShell => find_powershell(),
        }
    }
}

/// The environment variable that overrides bash discovery, standing in for
/// the `shellPath` setting pi reads from its settings file.
pub const BASH_PATH_VAR: &str = "TAU_BASH_PATH";

/// The environment variable that overrides PowerShell discovery.
pub const POWERSHELL_PATH_VAR: &str = "TAU_POWERSHELL_PATH";

/// Resolve bash: the environment override, then Git Bash in its usual
/// install locations, then `bash` on `PATH`, then `sh` (pi's order; on
/// Windows the last two are an error, since there is no `sh` to fall back
/// to).
pub fn find_bash() -> Result<Shell, String> {
    if let Some(from_env) = from_env(BASH_PATH_VAR, bash_shell) {
        return from_env;
    }

    #[cfg(windows)]
    {
        let mut searched = Vec::new();
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            let Ok(root) = env::var(variable) else {
                continue;
            };
            let candidate = Path::new(&root).join("Git").join("bin").join("bash.exe");
            if candidate.is_file() {
                return Ok(bash_shell(candidate));
            }
            searched.push(candidate);
        }
        // Cygwin, MSYS2, WSL and friends: whatever `where` finds first.
        if let Some(on_path) = find_on_path("bash.exe") {
            return Ok(bash_shell(on_path));
        }
        Err(format!(
            "No bash shell found. Options:\n  \
             1. Install Git for Windows: https://git-scm.com/download/win\n  \
             2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  \
             3. Set {BASH_PATH_VAR} to your bash.exe\n\n\
             Searched Git Bash in:\n{}",
            searched
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }

    #[cfg(not(windows))]
    {
        let plain = Path::new("/bin/bash");
        if plain.is_file() {
            return Ok(bash_shell(plain.to_path_buf()));
        }
        if let Some(on_path) = find_on_path("bash") {
            return Ok(bash_shell(on_path));
        }
        Ok(bash_shell(PathBuf::from("sh")))
    }
}

/// Resolve PowerShell: the environment override, then `pwsh.exe` (PowerShell
/// 7 and later), then `powershell.exe`.
pub fn find_powershell() -> Result<Shell, String> {
    if let Some(from_env) = from_env(POWERSHELL_PATH_VAR, powershell_shell) {
        return from_env;
    }
    for executable in ["pwsh.exe", "powershell.exe"] {
        if let Some(on_path) = find_on_path(executable) {
            return Ok(powershell_shell(on_path));
        }
    }
    Err("No PowerShell executable found. Install PowerShell or add powershell.exe/pwsh.exe to PATH.".to_string())
}

/// The shape of a shell given its program: `-c` for a real bash, `-s` plus
/// stdin for the WSL launcher, pi's PowerShell flags for PowerShell.
fn bash_shell(program: PathBuf) -> Shell {
    if is_legacy_wsl(&program) {
        Shell {
            program,
            args: vec!["-s".to_string()],
            stdin_transport: true,
        }
    } else {
        Shell {
            program,
            args: vec!["-c".to_string()],
            stdin_transport: false,
        }
    }
}

fn powershell_shell(program: PathBuf) -> Shell {
    Shell {
        program,
        args: [
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ]
        .iter()
        .map(|argument| (*argument).to_string())
        .collect(),
        stdin_transport: false,
    }
}

/// Windows' own `bash.exe` is the WSL launcher, which pi recognises by path
/// (`<drive>:\Windows\System32\bash.exe`, or Sysnative from a 32-bit
/// process) and hands the script to on stdin.
fn is_legacy_wsl(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('/', "\\").to_lowercase();
    let mut parts = text.split('\\');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(drive), Some(windows), Some(directory), Some(name), None)
            if drive.len() == 2
                && drive.ends_with(':')
                && windows == "windows"
                && (directory == "system32" || directory == "sysnative")
                && name == "bash.exe"
    )
}

/// The first match for `executable` on `PATH`, via `where` on Windows and
/// `which` elsewhere — the tools that already know about `PATHEXT` and
/// aliases, which is why pi shells out to them too. `where` can name files
/// that are not there, so on Windows the answer is verified.
fn find_on_path(executable: &str) -> Option<PathBuf> {
    let lister = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(lister).arg(executable).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let path = PathBuf::from(first);
    if cfg!(windows) && !path.is_file() {
        return None;
    }
    Some(path)
}

/// A shell path the user named, or `None` when they named none.
fn from_env(variable: &str, shape: impl Fn(PathBuf) -> Shell) -> Option<Result<Shell, String>> {
    let program = PathBuf::from(env::var_os(variable)?);
    Some(if program.is_file() {
        Ok(shape(program))
    } else {
        Err(format!(
            "Custom shell path not found: {}",
            program.display()
        ))
    })
}

/// Run a command through a shell — `bash` or `powershell`, the two differing
/// only in which shell is found and what is prepended to the command.
pub struct ShellTool {
    cwd: PathBuf,
    tier: Option<u8>,
    kind: ShellKind,
    /// A shell to run instead of discovering one: how an embedder pins a
    /// specific bash, and how the tests reach `bash -s` and friends.
    shell: Option<Shell>,
}

impl ShellTool {
    /// The `bash` tool, with paths resolving from `cwd`.
    pub fn bash(cwd: impl Into<PathBuf>, tier: Option<u8>) -> Self {
        Self::new(ShellKind::Bash, cwd, tier)
    }

    /// The `powershell` tool, with paths resolving from `cwd`.
    pub fn powershell(cwd: impl Into<PathBuf>, tier: Option<u8>) -> Self {
        Self::new(ShellKind::PowerShell, cwd, tier)
    }

    fn new(kind: ShellKind, cwd: impl Into<PathBuf>, tier: Option<u8>) -> Self {
        Self {
            cwd: cwd.into(),
            tier,
            kind,
            shell: None,
        }
    }

    /// Use `shell` instead of discovering one.
    pub fn with_shell(mut self, shell: Shell) -> Self {
        self.shell = Some(shell);
        self
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn def(&self) -> ToolDef {
        let config = self.kind.config();
        ToolDef {
            name: config.name.into(),
            description: format!(
                "Execute a {} command in the current working directory. Returns stdout and stderr. \
                 Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit \
                 first). If truncated, full output is saved to a temp file. Optionally provide a \
                 timeout in seconds.",
                config.shell_name,
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute"
                    },
                    "timeout": {
                        "type": "number",
                        "description": "Timeout in seconds (optional, no default timeout)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn demo_tier(&self) -> Option<u8> {
        self.tier
    }

    async fn execute(&self, arguments: Json) -> ToolOutput {
        let config = self.kind.config();
        let Some(command) = arguments.get("command").and_then(Json::as_str) else {
            return ToolOutput::err(format!("{} requires a command", config.name));
        };
        let timeout = match timeout_of(arguments.get("timeout")) {
            Ok(timeout) => timeout,
            Err(message) => return ToolOutput::err(message),
        };
        let shell = match &self.shell {
            Some(shell) => shell.clone(),
            None => match self.kind.discover() {
                Ok(shell) => shell,
                Err(message) => return ToolOutput::err(message),
            },
        };

        // The whole command is one string: the shell parses it, never tau.
        let command = format!("{}{command}", config.command_prefix);
        let cwd = self.cwd.clone();
        let outcome =
            tokio::task::spawn_blocking(move || run(&shell, config, &command, &cwd, timeout)).await;
        match outcome {
            Ok(outcome) if outcome.failed => ToolOutput::err(outcome.text),
            Ok(outcome) => ToolOutput::ok(outcome.text),
            Err(error) => ToolOutput::err(format!("{} task failed: {error}", config.shell_name)),
        }
    }
}

/// A validated `timeout:` — how long to wait, and the number as the model
/// wrote it, which the timeout message quotes back.
#[derive(Debug, Clone, Copy)]
struct Timeout {
    after: Duration,
    seconds: f64,
}

/// pi's timeout validation, message for message.
fn timeout_of(value: Option<&Json>) -> Result<Option<Timeout>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(seconds) = value.as_f64() else {
        return Err("Invalid timeout: must be a finite number of seconds".to_string());
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("Invalid timeout: must be a finite number of seconds".to_string());
    }
    if seconds * 1000.0 > MAX_TIMEOUT_MS {
        return Err(format!(
            "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
        ));
    }
    Ok(Some(Timeout {
        after: Duration::from_secs_f64(seconds),
        seconds,
    }))
}

/// What a finished command produced: the text for the model, notices and
/// status included, and whether it is an error.
#[derive(Debug)]
struct Outcome {
    text: String,
    failed: bool,
}

impl Outcome {
    fn failed(text: String) -> Self {
        Self { text, failed: true }
    }
}

/// Run `command` to completion, or until its timeout, and render the result.
fn run(
    shell: &Shell,
    config: ShellConfig,
    command: &str,
    cwd: &Path,
    timeout: Option<Timeout>,
) -> Outcome {
    if !cwd.is_dir() {
        return Outcome::failed(format!(
            "Working directory does not exist: {}\nCannot execute {} commands.",
            cwd.display(),
            config.shell_name
        ));
    }

    let mut builder = Command::new(&shell.program);
    builder.args(&shell.args);
    if !shell.stdin_transport {
        builder.arg(command);
    }
    builder.current_dir(cwd);
    builder.stdin(if shell.stdin_transport {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    builder.stdout(Stdio::piped()).stderr(Stdio::piped());
    // A process group of its own, so a timeout can take the whole tree with
    // it — see `kill_tree`.
    #[cfg(unix)]
    builder.process_group(0);

    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Outcome::failed(format!(
                "Cannot execute {} commands: {error}",
                config.shell_name
            ));
        }
    };
    if shell.stdin_transport
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(command.as_bytes());
    }

    // Both pipes feed one channel, so stdout and stderr merge in the order
    // the reads see them. The pump threads are not joined: a descendant that
    // inherited a pipe and outlives the shell keeps them parked in `read`
    // until it exits, and the accumulator is done with them either way.
    let (sender, receiver) = mpsc::channel::<Vec<u8>>();
    if let Some(stdout) = child.stdout.take() {
        let sender = sender.clone();
        thread::spawn(move || pump(stdout, &sender));
    }
    if let Some(stderr) = child.stderr.take() {
        let sender = sender.clone();
        thread::spawn(move || pump(stderr, &sender));
    }
    drop(sender);

    let mut output = Accumulator::new(env::temp_dir(), config.temp_prefix);
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match receiver.recv_timeout(POLL) {
            Ok(chunk) => output.append(&chunk),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => {
                kill_tree(&mut child);
                return Outcome::failed(format!(
                    "Cannot execute {} commands: {error}",
                    config.shell_name
                ));
            }
        }
        if let Some(timeout) = timeout
            && started.elapsed() >= timeout.after
        {
            timed_out = true;
            kill_tree(&mut child);
            let _ = wait_for_exit(&mut child);
            break None;
        }
    };

    // Output that arrives after the shell exited — a descendant still holds
    // the pipes — belongs to the command too, so keep reading until they go
    // quiet for a moment (pi's `waitForChildProcess`).
    while let Ok(chunk) = receiver.recv_timeout(EXIT_STDIO_GRACE) {
        output.append(&chunk);
    }

    let snapshot = output.snapshot();
    let code = status.as_ref().and_then(exit_code);
    let text = format_output(
        &snapshot,
        if !timed_out && code == Some(0) {
            "(no output)"
        } else {
            ""
        },
    );
    if timed_out {
        let seconds = timeout.map(|timeout| timeout.seconds).unwrap_or_default();
        return Outcome::failed(append_status(
            &text,
            &format!("Command timed out after {seconds} seconds"),
        ));
    }
    match code {
        Some(0) => Outcome {
            text,
            failed: false,
        },
        Some(code) => Outcome::failed(append_status(
            &text,
            &format!("Command exited with code {code}"),
        )),
        None => Outcome::failed(append_status(
            &text,
            "Command terminated without an exit code",
        )),
    }
}

/// The shell convention: an exit code, or `128 + signal` for a process that
/// was killed, so a signal death is never read as success (pi's rule).
fn exit_code(status: &ExitStatus) -> Option<i32> {
    if let Some(code) = status.code() {
        return Some(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        return status.signal().map(|signal| 128 + signal);
    }
    #[cfg(not(unix))]
    None
}

/// Read a pipe to its end, handing chunks on as they arrive.
fn pump(mut reader: impl Read, sender: &mpsc::Sender<Vec<u8>>) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if sender.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Wait for a killed process to be reaped, without trusting it to be prompt.
fn wait_for_exit(child: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + KILL_GRACE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => thread::sleep(POLL),
            Err(_) => return None,
        }
    }
    None
}

/// Kill a process and everything it started.
///
/// The shell is the least interesting part of the tree — the command's own
/// children are what keep running — so this kills by tree: `taskkill /T` on
/// Windows, run from `%SystemRoot%\System32` rather than from `PATH`, and the
/// process group on unix, which `process_group(0)` made the shell the leader
/// of.
fn kill_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(windows)]
    {
        let system_root =
            env::var_os("SystemRoot").unwrap_or_else(|| OsString::from("C:\\Windows"));
        let taskkill = Path::new(&system_root)
            .join("System32")
            .join("taskkill.exe");
        let _ = Command::new(taskkill)
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(unix)]
    {
        // SAFETY: `kill` is always safe to call with a signal number; the
        // worst a bad pid does is set `errno`.
        unsafe {
            if libc::kill(-(pid as i32), libc::SIGKILL) != 0 {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

/// pi's `formatOutput`: the kept output, or `empty_text` when there is none,
/// plus the notice that names which lines were dropped and where the whole
/// output went.
fn format_output(snapshot: &Snapshot, empty_text: &str) -> String {
    let truncation = &snapshot.truncation;
    let mut text = if truncation.content.is_empty() {
        empty_text.to_string()
    } else {
        truncation.content.clone()
    };
    if !truncation.truncated {
        return text;
    }

    let start_line = truncation.total_lines - truncation.output_lines + 1;
    let end_line = truncation.total_lines;
    let notice = if truncation.last_line_partial {
        format!(
            "Showing last {} of line {end_line} (line is {})",
            format_size(truncation.output_bytes),
            format_size(snapshot.last_line_bytes)
        )
    } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
        format!(
            "Showing lines {start_line}-{end_line} of {}",
            truncation.total_lines
        )
    } else {
        // The byte limit is pi's own constant here, not the effective one.
        format!(
            "Showing lines {start_line}-{end_line} of {} ({} limit)",
            truncation.total_lines,
            format_size(DEFAULT_MAX_BYTES)
        )
    };
    // pi always names the temp file; when writing it failed there is no file
    // to name, and saying nothing would hide output the model cannot see.
    let full = match (&snapshot.full_output_path, &snapshot.spill_error) {
        (Some(path), _) => format!(" Full output: {}", path.display()),
        (None, Some(error)) => format!(" Full output: unavailable ({error})"),
        (None, None) => String::new(),
    };
    text.push_str(&format!("\n\n[{notice}.{full}]"));
    text
}

/// pi's `appendStatus`: the status gets a paragraph of its own, after the
/// output — no blank line when there is no output.
fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    async fn bash(dir: &Path, args: Json) -> ToolOutput {
        ShellTool::bash(dir, None).execute(args).await
    }

    fn command(text: &str) -> Json {
        serde_json::json!({ "command": text })
    }

    #[tokio::test]
    async fn a_command_runs_and_its_output_comes_back_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let out = bash(tmp.path(), command("echo hello")).await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(out.text(), "hello\n");
    }

    #[tokio::test]
    async fn stderr_is_merged_into_the_output() {
        let tmp = tempfile::tempdir().unwrap();
        let out = bash(tmp.path(), command("echo out; echo err 1>&2")).await;
        assert!(!out.is_error, "{}", out.text());
        let body = out.text();
        assert!(body.contains("out"), "{body}");
        assert!(body.contains("err"), "{body}");
    }

    #[tokio::test]
    async fn the_command_runs_in_the_tools_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let out = bash(tmp.path(), command("pwd")).await;
        // Git Bash prints a posix path for the same directory, so match on
        // the temp directory's own name rather than on its spelling.
        let name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(out.text().contains(&name), "{}", out.text());
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_an_error_result_naming_the_code() {
        let tmp = tempfile::tempdir().unwrap();
        let out = bash(tmp.path(), command("echo boom; exit 3")).await;
        assert!(out.is_error);
        // The output already ends in a newline and pi puts a blank line
        // between it and the status, so there are two of them. Mirrored
        // rather than tidied: the text is what pi hands the model.
        assert_eq!(out.text(), "boom\n\n\nCommand exited with code 3");
    }

    #[tokio::test]
    async fn a_command_with_no_output_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let out = bash(tmp.path(), command("true")).await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(out.text(), "(no output)");
    }

    #[tokio::test]
    async fn a_silent_failure_is_just_the_status() {
        let tmp = tempfile::tempdir().unwrap();
        let out = bash(tmp.path(), command("exit 4")).await;
        assert!(out.is_error);
        assert_eq!(out.text(), "Command exited with code 4");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_command_and_quotes_the_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out = bash(
            tmp.path(),
            serde_json::json!({ "command": "echo before; sleep 30", "timeout": 1 }),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.text(), "before\n\n\nCommand timed out after 1 seconds");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_timeout_takes_the_commands_children_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("marker.txt");
        let out = bash(
            tmp.path(),
            serde_json::json!({
                // A grandchild that would write the marker two seconds from
                // now, long after the timeout fires.
                "command": "( sleep 2; touch marker.txt ) & sleep 30",
                "timeout": 1
            }),
        )
        .await;
        assert!(out.is_error, "{}", out.text());
        // Give the grandchild every chance to be alive before believing it
        // is dead.
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            !marker.exists(),
            "the timed-out command's child outlived the kill"
        );
    }

    #[tokio::test]
    async fn an_invalid_timeout_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        let zero = bash(
            tmp.path(),
            serde_json::json!({ "command": "true", "timeout": 0 }),
        )
        .await;
        assert!(zero.is_error);
        assert_eq!(
            zero.text(),
            "Invalid timeout: must be a finite number of seconds"
        );

        let huge = bash(
            tmp.path(),
            serde_json::json!({ "command": "true", "timeout": 1_000_000_000 }),
        )
        .await;
        assert!(huge.is_error);
        assert_eq!(
            huge.text(),
            "Invalid timeout: maximum is 2147483.647 seconds"
        );
    }

    #[tokio::test]
    async fn a_missing_command_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        let out = bash(tmp.path(), serde_json::json!({})).await;
        assert!(out.is_error);
        assert_eq!(out.text(), "bash requires a command");
    }

    #[tokio::test]
    async fn a_missing_working_directory_is_an_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("gone");
        let out = bash(&gone, command("true")).await;
        assert!(out.is_error);
        assert_eq!(
            out.text(),
            format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                gone.display()
            )
        );
    }

    #[tokio::test]
    async fn output_past_the_line_limit_is_truncated_to_a_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let out = bash(tmp.path(), command("seq 1 3000")).await;
        assert!(!out.is_error, "{}", out.text());
        let body = out.text();
        // The tail is what is kept, and the notice names the whole range.
        assert!(
            body.starts_with("1001\n"),
            "{}",
            &body[..40.min(body.len())]
        );
        assert!(
            body.contains("[Showing lines 1001-3000 of 3000. Full output: "),
            "{body}"
        );
        let path = body
            .rsplit("Full output: ")
            .next()
            .unwrap()
            .trim_end_matches(']')
            .to_string();
        let spilled = std::fs::read_to_string(&path).unwrap();
        assert!(spilled.starts_with("1\n"));
        assert!(spilled.ends_with("3000\n"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_wsl_launcher_is_recognised_and_reads_the_command_from_stdin() {
        let wsl = Path::new("C:\\")
            .join("Windows")
            .join("System32")
            .join("bash.exe");
        let shell = bash_shell(wsl.clone());
        assert!(shell.stdin_transport);
        assert_eq!(shell.args, vec!["-s".to_string()]);

        // Forward slashes and any case normalize the same way.
        let sysnative = bash_shell(PathBuf::from("c:/windows/sysnative/bash.exe"));
        assert!(sysnative.stdin_transport);

        let git_bash = bash_shell(PathBuf::from("C:\\Program Files\\Git\\bin\\bash.exe"));
        assert!(!git_bash.stdin_transport);
        assert_eq!(git_bash.args, vec!["-c".to_string()]);

        // A file merely *named* bash.exe somewhere else is a real bash.
        assert!(!is_legacy_wsl(Path::new("C:\\tools\\bin\\bash.exe")));
        assert!(!is_legacy_wsl(Path::new(
            "C:\\Windows\\System32\\bash.exe.bak"
        )));
        assert!(!is_legacy_wsl(&wsl.join("extra")));
    }

    #[test]
    fn the_two_tools_announce_themselves_differently() {
        let bash = ShellTool::bash(".", None).def();
        assert_eq!(bash.name, "bash");
        assert!(
            bash.description
                .starts_with("Execute a bash command in the current working directory."),
            "{}",
            bash.description
        );
        assert!(
            bash.description.contains("last 2000 lines or 50KB"),
            "{}",
            bash.description
        );

        let powershell = ShellTool::powershell(".", None).def();
        assert_eq!(powershell.name, "powershell");
        assert!(
            powershell
                .description
                .starts_with("Execute a PowerShell command"),
            "{}",
            powershell.description
        );
        assert_eq!(powershell.parameters["required"][0], "command");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_runs_a_command() {
        let tmp = tempfile::tempdir().unwrap();
        let out = ShellTool::powershell(tmp.path(), None)
            .execute(command("[Console]::Out.Write('hi')"))
            .await;
        assert!(!out.is_error, "{}", out.text());
        assert_eq!(out.text(), "hi");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_errors_name_powershell() {
        let tmp = tempfile::tempdir().unwrap();
        let out = ShellTool::powershell(tmp.path(), None)
            .execute(serde_json::json!({}))
            .await;
        assert!(out.is_error);
        assert_eq!(out.text(), "powershell requires a command");
    }
}
