//! Actor CLI launchers, port of `src/main/buddy/launchers.ts`.
//!
//! Builds per-actor command lines (native claude/codex/cursor/agy/opencode/
//! kimi modes vs. generic "contract" commands driven by `BUDDY_*` env vars)
//! and runs them: piped stdio via `tokio::process` for most actors, a PTY via
//! `portable-pty` for CLIs (opencode) that hang without a TTY.
//!
//! This module exposes the canonical shared helpers (`split_command`,
//! `command_kind_for`, the `is_wecode_*` detectors); `store.rs` and
//! `model_detect.rs` currently carry private duplicates that a later
//! integration wave will rewire to these.

use crate::buddy::shell_path::{install_hint_for, merge_child_env};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LauncherCommandKind {
    NativeClaude,
    NativeCodex,
    NativeCursor,
    NativeAgy,
    NativeOpencode,
    NativeKimi,
    Contract,
}

impl LauncherCommandKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LauncherCommandKind::NativeClaude => "native_claude",
            LauncherCommandKind::NativeCodex => "native_codex",
            LauncherCommandKind::NativeCursor => "native_cursor",
            LauncherCommandKind::NativeAgy => "native_agy",
            LauncherCommandKind::NativeOpencode => "native_opencode",
            LauncherCommandKind::NativeKimi => "native_kimi",
            LauncherCommandKind::Contract => "contract",
        }
    }
}

/// Input for [`build_launcher_command`], mirroring the Electron edition's
/// `LauncherCommandInput`.
#[derive(Debug, Clone, Default)]
pub struct LauncherCommandInput {
    pub actor: String,
    pub command: String,
    pub mode: Option<String>,
    pub prompt_file: String,
    pub prompt_text: Option<String>,
    pub event_file: Option<String>,
    pub output_file: Option<String>,
    pub repo_root: Option<String>,
    pub task_dir: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    /// Buddy launcher timeout; native_agy maps this to --print-timeout (agy default is only 5m).
    pub timeout_seconds: Option<u64>,
}

/// A fully-built launcher invocation, mirroring `LauncherCommand`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherCommand {
    pub command: String,
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
    pub kind: LauncherCommandKind,
    pub stdin_text: Option<String>,
}

/// Result of a launcher run (piped or PTY), mirroring the Electron edition's
/// `LauncherRunResult` (`{ exitCode, signal, timedOut? }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherRunResult {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    /// Set only when Buddy's deadline fired, not on user abort or external
    /// signals.
    pub timed_out: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LauncherError {
    /// Command not found in PATH (message includes an install hint when known).
    #[error("{0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("pty error: {0}")]
    Pty(String),
}

/// Whether the given command kind requires a PTY to function correctly.
/// opencode CLI hangs when spawned with piped stdio (no TTY); it needs a PTY
/// to produce output in `--format json` mode.
pub fn kind_needs_pty(kind: LauncherCommandKind) -> bool {
    kind == LauncherCommandKind::NativeOpencode
}

/// Map a command kind to the parser actor name for correct output parsing.
/// When the command is opencode but the actor is kimi (e.g.
/// `opencode -m provider/kimi-k2.6`), the output format is opencode's JSON,
/// so we need the opencode parser.
pub fn parser_actor_for_kind(actor: &str, kind: LauncherCommandKind) -> String {
    match kind {
        LauncherCommandKind::NativeOpencode => "opencode".to_string(),
        LauncherCommandKind::NativeKimi => "kimi".to_string(),
        LauncherCommandKind::NativeClaude => "claude".to_string(),
        LauncherCommandKind::NativeCodex => "codex".to_string(),
        LauncherCommandKind::NativeCursor => "cursor".to_string(),
        LauncherCommandKind::NativeAgy => "agy".to_string(),
        LauncherCommandKind::Contract => actor.to_string(),
    }
}

/// Port of `splitCommand`: splits on whitespace while keeping double-quoted
/// segments together (matching `(?:[^\s"]+|"[^"]*")+`), then strips one
/// leading and one trailing quote per token. If nothing matches, returns the
/// raw command as a single token (the JS `?? [command]` fallback).
pub fn split_command(command: &str) -> Vec<String> {
    let pattern = split_pattern();
    let tokens: Vec<String> = pattern
        .find_iter(command)
        .map(|m| {
            let part = m.as_str();
            // replace(/^"|"$/g, '')
            let part = part.strip_prefix('"').unwrap_or(part);
            part.strip_suffix('"').unwrap_or(part).to_string()
        })
        .collect();
    if tokens.is_empty() {
        return vec![command.to_string()];
    }
    tokens
}

fn split_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r#"(?:[^\s"]+|"[^"]*")+"#).unwrap())
}

/// `basename(baseCmd[0] ?? '')` — final path component, or the token itself
/// when it has none (matches Node's `path.basename` for the inputs we see).
fn basename(token: &str) -> &str {
    std::path::Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token)
}

/// Map an actor + command string to its launcher kind. Splits the command
/// first; see [`command_kind_for_tokens`] for the token-level logic.
pub fn command_kind_for(actor: &str, command: &str) -> LauncherCommandKind {
    command_kind_for_tokens(actor, &split_command(command))
}

/// Token-level variant of [`command_kind_for`] (the TS signature accepts
/// `string | string[]`; this covers the array form).
///
/// Detects the native CLI by executable name first, regardless of actor name.
/// This allows e.g. actor=`kimi` with command=`opencode -m provider/kimi-k2.6`
/// to be correctly identified as `native_opencode`.
///
/// WeCode wraps both claude and codex: `wecode` (or `wecode ...` without a
/// leading `codex` token) runs claude; `wecode codex ...` runs codex.
pub fn command_kind_for_tokens(actor: &str, base_cmd: &[String]) -> LauncherCommandKind {
    let executable = basename(base_cmd.first().map(String::as_str).unwrap_or(""));
    if executable == "claude" || is_wecode_claude_command_tokens(base_cmd) {
        return LauncherCommandKind::NativeClaude;
    }
    if executable == "codex"
        || (executable == "wecode" && base_cmd.get(1).map(String::as_str) == Some("codex"))
    {
        return LauncherCommandKind::NativeCodex;
    }
    if executable == "cursor-agent" || executable == "agent" {
        return LauncherCommandKind::NativeCursor;
    }
    if executable == "agy" || executable == "antigravity" {
        return LauncherCommandKind::NativeAgy;
    }
    if executable == "opencode" {
        return LauncherCommandKind::NativeOpencode;
    }
    if executable == "kimi" {
        return LauncherCommandKind::NativeKimi;
    }
    // Fallback: when no command is specified, infer from actor name.
    // Also: the Antigravity settings card always speaks agy's native protocol.
    // Never fall through to contract flags (--actor, etc.) just because the
    // command string used a wrapper basename we do not recognize — that is
    // exactly what produces "flags provided but not defined: -actor".
    if executable.is_empty() || executable == "wecode" || actor == "agy" {
        match actor {
            "claude" => return LauncherCommandKind::NativeClaude,
            "codex" => return LauncherCommandKind::NativeCodex,
            "cursor" => return LauncherCommandKind::NativeCursor,
            "agy" => return LauncherCommandKind::NativeAgy,
            "opencode" => return LauncherCommandKind::NativeOpencode,
            "kimi" => return LauncherCommandKind::NativeKimi,
            _ => {}
        }
    }
    LauncherCommandKind::Contract
}

/// Whether a command invokes WeCode (the `wecode` executable), regardless of
/// path or arguments. Detection mirrors [`command_kind_for`]: split the
/// command, take basename of the first token, compare to `wecode`. Does NOT
/// depend on any permission flag.
pub fn is_wecode_command(command: &str) -> bool {
    is_wecode_command_tokens(&split_command(command))
}

pub fn is_wecode_command_tokens(base_cmd: &[String]) -> bool {
    basename(base_cmd.first().map(String::as_str).unwrap_or("")) == "wecode"
}

/// Whether a command invokes WeCode Claude (i.e. `wecode` whose second token
/// is NOT `codex`). Mirrors the WeCode-Claude branch of `commandKindFor`.
pub fn is_wecode_claude_command(command: &str) -> bool {
    is_wecode_claude_command_tokens(&split_command(command))
}

pub fn is_wecode_claude_command_tokens(base_cmd: &[String]) -> bool {
    if !is_wecode_command_tokens(base_cmd) {
        return false;
    }
    base_cmd.get(1).map(String::as_str) != Some("codex")
}

/// Whether a command invokes WeCode Codex (i.e. `wecode codex ...`).
/// Mirrors the WeCode-Codex branch of `commandKindFor`.
pub fn is_wecode_codex_command(command: &str) -> bool {
    is_wecode_codex_command_tokens(&split_command(command))
}

pub fn is_wecode_codex_command_tokens(base_cmd: &[String]) -> bool {
    is_wecode_command_tokens(base_cmd) && base_cmd.get(1).map(String::as_str) == Some("codex")
}

/// Strips legacy bare flags (`--full-auto`) from a codex base command.
fn clean_codex_base_command(base_cmd: &[String]) -> Vec<String> {
    let mut cleaned = Vec::with_capacity(base_cmd.len());
    if let Some(first) = base_cmd.first() {
        cleaned.push(first.clone());
    }
    for part in &base_cmd[base_cmd.len().min(1)..] {
        if part != "--full-auto" {
            cleaned.push(part.clone());
        }
    }
    cleaned
}

/// Build the full command line for an actor invocation. Port of
/// `buildLauncherCommand`; see the Electron edition for the per-actor flag
/// rationale.
pub fn build_launcher_command(input: &LauncherCommandInput) -> LauncherCommand {
    let mut base_cmd = split_command(&input.command);
    let kind = command_kind_for_tokens(&input.actor, &base_cmd);
    if base_cmd.first().map(String::is_empty).unwrap_or(true)
        && kind != LauncherCommandKind::Contract
    {
        base_cmd = vec![input.actor.clone()];
    }
    let base_cmd = if kind == LauncherCommandKind::NativeCodex {
        clean_codex_base_command(&base_cmd)
    } else {
        base_cmd
    };
    let command = base_cmd.first().cloned().unwrap_or_default();
    let prefix_args: Vec<String> = base_cmd.iter().skip(1).cloned().collect();

    match kind {
        LauncherCommandKind::NativeClaude => {
            let mut args = prefix_args;
            args.extend([
                "-p".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
                "--input-format".to_string(),
                "text".to_string(),
            ]);
            if let Some(session_id) = &input.session_id {
                args.push("--resume".to_string());
                args.push(session_id.clone());
            }
            LauncherCommand {
                command,
                args,
                env: None,
                kind,
                stdin_text: input.prompt_text.clone(),
            }
        }
        LauncherCommandKind::NativeCodex => {
            let mut args = prefix_args;
            args.extend([
                "exec".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "--json".to_string(),
                "--skip-git-repo-check".to_string(),
            ]);
            if let Some(repo_root) = &input.repo_root {
                args.push("-C".to_string());
                args.push(repo_root.clone());
            }
            if let Some(output_file) = &input.output_file {
                args.push("-o".to_string());
                args.push(output_file.clone());
            }
            if let Some(session_id) = &input.session_id {
                args.push("resume".to_string());
                args.push(session_id.clone());
            }
            args.push("-".to_string());
            LauncherCommand {
                command,
                args,
                env: None,
                kind,
                stdin_text: input.prompt_text.clone(),
            }
        }
        LauncherCommandKind::NativeCursor => {
            let prompt_text = input
                .prompt_text
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            let mut args = prefix_args;
            args.extend([
                "--print".to_string(),
                "--force".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                // Re-enabled for live progress. Consumers must coalesce
                // deltas (not one event per UI line) and prefer
                // result.result for the final reply.
                "--stream-partial-output".to_string(),
                // Buddy owns the next turn. Finish this turn (including
                // subagents) without waiting for background shells.
                // Persistent services must be detached via the Buddy
                // service commands: CLI cleanup stops shells it still owns
                // on exit.
                "--single-turn".to_string(),
            ]);
            if let Some(session_id) = &input.session_id {
                args.push("--resume".to_string());
                args.push(session_id.clone());
            }
            args.push(prompt_text);
            LauncherCommand {
                command,
                args,
                env: None,
                kind,
                stdin_text: None,
            }
        }
        LauncherCommandKind::NativeAgy => {
            // agy requires the prompt attached to -p= / --print=. Bare --print
            // steals the next flag as the prompt. Long Buddy prompts go via
            // stdin stream-json instead. Default print-timeout is only 5m —
            // always override from Buddy's timeout.
            let timeout_seconds = input.timeout_seconds.unwrap_or(7200).max(1);
            let prompt_text = input.prompt_text.clone().unwrap_or_default();
            let stdin_payload = format!(
                "{}\n",
                serde_json::json!({
                    "event": "user",
                    "message": { "content": prompt_text }
                })
            );
            let mut args = prefix_args;
            args.extend([
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--input-format".to_string(),
                "stream-json".to_string(),
                "--dangerously-skip-permissions".to_string(),
                format!("--print-timeout={timeout_seconds}s"),
            ]);
            if let Some(session_id) = &input.session_id {
                args.push("--conversation".to_string());
                args.push(session_id.clone());
            }
            args.push("-p=".to_string());
            LauncherCommand {
                command,
                args,
                env: None,
                kind,
                stdin_text: Some(stdin_payload),
            }
        }
        LauncherCommandKind::NativeOpencode => {
            let mut args = prefix_args;
            args.extend([
                "run".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]);
            if let Some(session_id) = &input.session_id {
                args.push("--session".to_string());
                args.push(session_id.clone());
            }
            let prompt_text = input.prompt_text.as_deref().map(str::trim).unwrap_or("");
            if !prompt_text.is_empty() {
                args.push(prompt_text.to_string());
            }
            LauncherCommand {
                command,
                args,
                env: None,
                kind,
                stdin_text: None,
            }
        }
        LauncherCommandKind::NativeKimi => {
            let prompt_text = input
                .prompt_text
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            let mut args = prefix_args;
            args.extend([
                "-p".to_string(),
                prompt_text,
                "--output-format".to_string(),
                "stream-json".to_string(),
            ]);
            if let Some(session_id) = &input.session_id {
                args.push("-S".to_string());
                args.push(session_id.clone());
            }
            LauncherCommand {
                command,
                args,
                env: None,
                kind,
                stdin_text: None,
            }
        }
        LauncherCommandKind::Contract => {
            let mode = input
                .mode
                .clone()
                .unwrap_or_else(|| if input.session_id.is_some() { "resume" } else { "start" }.to_string());
            let repo_root = input.repo_root.clone().unwrap_or_default();
            let task_dir = input.task_dir.clone().unwrap_or_default();
            let run_id = input.run_id.clone().unwrap_or_default();
            let output_file = input.output_file.clone().unwrap_or_default();
            let event_file = input.event_file.clone().unwrap_or_default();
            let mut env = HashMap::new();
            env.insert("BUDDY_ACTOR".to_string(), input.actor.clone());
            env.insert("BUDDY_MODE".to_string(), mode.clone());
            env.insert("BUDDY_REPO_ROOT".to_string(), repo_root.clone());
            env.insert("BUDDY_TASK_DIR".to_string(), task_dir.clone());
            env.insert("BUDDY_RUN_ID".to_string(), run_id.clone());
            env.insert("BUDDY_PROMPT_FILE".to_string(), input.prompt_file.clone());
            env.insert("BUDDY_OUTPUT_FILE".to_string(), output_file.clone());
            env.insert("BUDDY_EVENT_FILE".to_string(), event_file.clone());
            env.insert(
                "BUDDY_SESSION_ID".to_string(),
                input.session_id.clone().unwrap_or_default(),
            );
            let mut args = prefix_args;
            args.extend([
                "--actor".to_string(),
                input.actor.clone(),
                "--mode".to_string(),
                mode,
                "--repo-root".to_string(),
                repo_root,
                "--task-dir".to_string(),
                task_dir,
                "--run-id".to_string(),
                run_id,
                "--prompt-file".to_string(),
                input.prompt_file.clone(),
                "--output-file".to_string(),
                output_file,
                "--event-file".to_string(),
                event_file,
            ]);
            if let Some(session_id) = &input.session_id {
                args.push("--session-id".to_string());
                args.push(session_id.clone());
            }
            LauncherCommand {
                command,
                args,
                env: Some(env),
                kind,
                stdin_text: None,
            }
        }
    }
}

/// Input for [`run_launcher`] (piped stdio), mirroring the Electron
/// edition's `runLauncher` input.
#[derive(Debug, Clone, Default)]
pub struct RunLauncherInput {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Option<HashMap<String, String>>,
    pub stdin_text: Option<String>,
    pub timeout_ms: u64,
    /// Abort handle, port of the TS `signal?: AbortSignal`: when the flag is
    /// set the child receives SIGTERM and the function still resolves
    /// normally — the caller checks the flag to detect the abort.
    pub abort: Option<Arc<AtomicBool>>,
}

/// Input for [`run_launcher_with_pty`], mirroring `runLauncherWithPty`.
#[derive(Debug, Clone, Default)]
pub struct PtyRunInput {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Option<HashMap<String, String>>,
    pub timeout_ms: u64,
    /// See [`RunLauncherInput::abort`].
    pub abort: Option<Arc<AtomicBool>>,
}

/// Resolves once `flag` turns true; pending forever when `None`. The TS
/// edition hooks `signal.addEventListener('abort', ...)`; polling the flag
/// is the dependency-free equivalent.
async fn wait_for_abort(flag: Option<Arc<AtomicBool>>) {
    match flag {
        Some(flag) => {
            // An already-set flag aborts immediately (TS: `signal.aborted`
            // check at registration time).
            while !flag.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// Split stdout/stderr chunks into complete lines, keeping a trailing partial
/// line across chunk boundaries so NDJSON events are not broken mid-object
/// (port of `createLineSplitter`).
struct LineSplitter<F: FnMut(String)> {
    buffer: String,
    on_line: F,
}

impl<F: FnMut(String)> LineSplitter<F> {
    fn new(on_line: F) -> Self {
        LineSplitter {
            buffer: String::new(),
            on_line,
        }
    }

    fn push(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.buffer.push_str(chunk);
        while let Some(pos) = self.buffer.find('\n') {
            let mut line = self.buffer.drain(..pos).collect::<String>();
            if line.ends_with('\r') {
                line.pop();
            }
            self.buffer.drain(..1);
            if !line.is_empty() {
                (self.on_line)(line);
            }
        }
    }

    fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.buffer);
        (self.on_line)(line);
    }
}

/// Cap how long we wait for stdout/stderr to close after the child exits
/// (port of `streamDrainMs`).
pub fn stream_drain_ms(timeout_ms: u64) -> u64 {
    if timeout_ms == 0 {
        return 250;
    }
    timeout_ms.clamp(50, 500)
}

/// Run a launcher command with piped stdio. Port of `runLauncher`:
/// - stdout/stderr go through `Utf8StreamDecoder` so a multi-byte UTF-8
///   sequence split across two reads decodes intact (Node string_decoder
///   parity), then through `LineSplitter` so NDJSON events split across
///   chunks are reassembled before parsing.
/// - `stdin_text` is written to stdin, which is then closed; a broken pipe
///   (child exited early) is swallowed like the TS EPIPE guard.
/// - on timeout the child receives SIGTERM (then SIGKILL after 1.5s, since
///   cancellation must settle even when a CLI ignores SIGTERM) and we await
///   its exit. `timed_out` is only set when Buddy's deadline fired, not on
///   user abort.
/// - after the child exits, draining inherited pipes is bounded by
///   `stream_drain_ms`: hung grandchild pipes cannot stall the runner.
pub async fn run_launcher<F, G>(
    input: &RunLauncherInput,
    on_stdout: F,
    on_stderr: G,
) -> Result<LauncherRunResult, LauncherError>
where
    F: FnMut(String) + Send,
    G: FnMut(String) + Send,
{
    let tokens = split_command(&input.command);
    let command = tokens.first().cloned().unwrap_or_default();
    let prefix_args = &tokens[tokens.len().min(1)..];

    let mut cmd = tokio::process::Command::new(&command);
    cmd.args(prefix_args)
        .args(&input.args)
        .current_dir(&input.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // TS: `env: mergeChildEnv(process.env, input.env)` — the child sees the
    // full process env with the launcher overlay applied and proxy pairs
    // mirrored across casings.
    let merged_env = merge_child_env(&std::env::vars().collect(), input.env.as_ref());
    cmd.env_clear().envs(merged_env);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(command_not_found_error(&command));
        }
        Err(error) => return Err(LauncherError::Io(error)),
    };

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let mut stdin = child.stdin.take().expect("stdin piped");
    let pid = child.id();

    type IoFuture<'a> =
        std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>>;

    let mut stdout_reader: Option<IoFuture<'_>> = Some(Box::pin(async move {
        let mut splitter = LineSplitter::new(on_stdout);
        let mut buf = [0u8; 8192];
        let mut decoder = Utf8StreamDecoder::default();
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) => {
                    splitter.push(&decoder.finish());
                    splitter.flush();
                    return Ok(());
                }
                Ok(n) => splitter.push(&decoder.push(&buf[..n])),
                Err(error) => {
                    splitter.flush();
                    return Err(error);
                }
            }
        }
    }));
    let mut stderr_reader: Option<IoFuture<'_>> = Some(Box::pin(async move {
        let mut splitter = LineSplitter::new(on_stderr);
        let mut buf = [0u8; 8192];
        let mut decoder = Utf8StreamDecoder::default();
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) => {
                    splitter.push(&decoder.finish());
                    splitter.flush();
                    return Ok(());
                }
                Ok(n) => splitter.push(&decoder.push(&buf[..n])),
                Err(error) => {
                    splitter.flush();
                    return Err(error);
                }
            }
        }
    }));

    // Write prompt text to stdin, then close the writable side. The child may
    // exit before we finish writing (e.g. wecode auto-upgrades and relaunches
    // itself, closing the pipe) — swallow EPIPE like the Electron edition.
    let stdin_text = input.stdin_text.clone().unwrap_or_default();
    let mut stdin_writer: Option<IoFuture<'_>> = Some(Box::pin(async move {
        let result = stdin.write_all(stdin_text.as_bytes()).await;
        drop(stdin);
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(error) => Err(error),
        }
    }));

    // Phase 1: drive the IO futures while waiting for the child to exit (or
    // for the deadline / abort flag to fire). Completes on exit alone —
    // readers may still have buffered data or hung grandchild writers.
    let mut timed_out = false;
    let status = {
        let timeout = tokio::time::sleep(std::time::Duration::from_millis(input.timeout_ms));
        tokio::pin!(timeout);
        let abort = wait_for_abort(input.abort.clone());
        tokio::pin!(abort);
        enum Trigger {
            Exit(std::io::Result<std::process::ExitStatus>),
            Timeout,
            Abort,
            Io(std::io::Result<()>),
        }
        let mut wait = Box::pin(child.wait());
        let trigger = loop {
            tokio::select! {
                status = &mut wait => break Trigger::Exit(status),
                _ = &mut timeout => break Trigger::Timeout,
                // Abort: SIGTERM like the TS `onAbort`, then resolve
                // normally — the caller checks the abort flag.
                _ = &mut abort => break Trigger::Abort,
                result = async { stdout_reader.as_mut().expect("polled while Some").await }, if stdout_reader.is_some() => {
                    stdout_reader = None;
                    if let Err(error) = &result { break Trigger::Io(Err(std::io::Error::new(error.kind(), error.to_string()))); }
                }
                result = async { stderr_reader.as_mut().expect("polled while Some").await }, if stderr_reader.is_some() => {
                    stderr_reader = None;
                    if let Err(error) = &result { break Trigger::Io(Err(std::io::Error::new(error.kind(), error.to_string()))); }
                }
                result = async { stdin_writer.as_mut().expect("polled while Some").await }, if stdin_writer.is_some() => {
                    stdin_writer = None;
                    if let Err(error) = &result { break Trigger::Io(Err(std::io::Error::new(error.kind(), error.to_string()))); }
                }
            }
        };
        match trigger {
            Trigger::Exit(status) => status?,
            Trigger::Io(result) => result.map(|_| unreachable!("no exit status")).map_err(LauncherError::Io)?,
            Trigger::Timeout | Trigger::Abort => {
                if matches!(trigger, Trigger::Timeout) {
                    timed_out = input
                        .abort
                        .as_ref()
                        .map(|flag| !flag.load(Ordering::SeqCst))
                        .unwrap_or(true);
                }
                send_sigterm(pid);
                // The CLI may ignore SIGTERM; escalate to SIGKILL so the
                // deadline always settles (TS: the forceKill timer).
                match tokio::time::timeout(std::time::Duration::from_millis(1500), &mut wait).await
                {
                    Ok(status) => status?,
                    Err(_) => {
                        send_sigkill(pid);
                        wait.await?
                    }
                }
            }
        }
    };

    // Phase 2: the deadline covers the launcher, not draining inherited pipes
    // after exit. Bound the drain: grandchildren may keep pipes open after
    // the launcher exits. On expiry the reader futures are dropped, which
    // closes the pipes (TS: `destroyStream`).
    let drain = async move {
        let out = match stdout_reader {
            Some(future) => future.await,
            None => Ok(()),
        };
        let err = match stderr_reader {
            Some(future) => future.await,
            None => Ok(()),
        };
        let inp = match stdin_writer {
            Some(future) => future.await,
            None => Ok(()),
        };
        (out, err, inp)
    };
    match tokio::time::timeout(
        std::time::Duration::from_millis(stream_drain_ms(input.timeout_ms)),
        drain,
    )
    .await
    {
        Ok((out_result, err_result, in_result)) => {
            out_result?;
            err_result?;
            in_result?;
        }
        Err(_) => {}
    }

    let mut result = exit_result_from_status(&status);
    result.timed_out = timed_out;
    Ok(result)
}

/// TS `LauncherTimeoutError` message (`Actor timed out after N seconds`).
pub fn launcher_timeout_message(timeout_ms: u64) -> String {
    format!("Actor timed out after {} seconds", timeout_ms / 1000)
}

#[cfg(unix)]
fn exit_result_from_status(status: &std::process::ExitStatus) -> LauncherRunResult {
    use std::os::unix::process::ExitStatusExt;
    LauncherRunResult {
        exit_code: status.code(),
        signal: status.signal().map(signal_name),
        timed_out: false,
    }
}

#[cfg(not(unix))]
fn exit_result_from_status(status: &std::process::ExitStatus) -> LauncherRunResult {
    LauncherRunResult {
        exit_code: status.code(),
        signal: None,
        timed_out: false,
    }
}

/// Node-style signal names for the signals a launcher is likely to die from.
#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        other => return other.to_string(),
    }
    .to_string()
}

/// Send SIGTERM to a process by id. `libc` is not a direct dependency, so we
/// shell out to `kill(1)`; failures are ignored (the wait fallback still
/// reaps the child, matching the Electron edition's fire-and-forget kill).
fn send_sigterm(pid: Option<u32>) {
    send_signal(pid, "-TERM");
}

/// Send SIGKILL to a process by id (the TS `forceKill` escalation).
fn send_sigkill(pid: Option<u32>) {
    send_signal(pid, "-KILL");
}

fn send_signal(pid: Option<u32>, signal: &str) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill")
            .args([signal, &pid.to_string()])
            .status();
    }
}

/// ANSI escape sequence pattern for stripping TTY output.
fn ansi_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new("\x1b\\[[0-9;]*[a-zA-Z]|\x1b\\].*?(?:\x07|\x1b\\\\)").unwrap())
}

/// Strip ANSI escape codes and carriage returns before forwarding PTY data.
fn clean_pty_output(data: &str) -> String {
    ansi_pattern()
        .replace_all(data, "")
        .replace("\r\n", "\n")
        .replace('\r', "")
}

/// Incremental UTF-8 decoder for streamed child output.
///
/// Child stdout/stderr (and PTY master) is read in fixed-size byte chunks
/// that can split a multi-byte UTF-8 sequence across two reads. Decoding each
/// chunk independently with `from_utf8_lossy` corrupts the split character
/// into two U+FFFD replacement chars (the `��` mojibake seen in actor
/// transcripts). The Electron edition never hit this because Node's
/// string_decoder buffers partial sequences across `data` events; this
/// decoder restores that behavior by holding back an incomplete trailing
/// sequence and prepending it to the next chunk. Genuinely invalid bytes are
/// still replaced with U+FFFD, matching `from_utf8_lossy`.
#[derive(Default)]
struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    /// Decode a chunk, buffering any incomplete trailing sequence.
    fn push(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        self.decode(false)
    }

    /// Flush at EOF: a leftover incomplete sequence is invalid input.
    fn finish(&mut self) -> String {
        self.decode(true)
    }

    fn decode(&mut self, flush: bool) -> String {
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(valid) => {
                    out.push_str(valid);
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    out.push_str(
                        std::str::from_utf8(&self.pending[..valid_up_to]).expect("valid prefix"),
                    );
                    match error.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid_up_to + len);
                        }
                        None => {
                            // Incomplete sequence at the end of the buffer:
                            // keep it (at most 3 bytes) for the next chunk.
                            self.pending.drain(..valid_up_to);
                            if flush {
                                out.push('\u{FFFD}');
                                self.pending.clear();
                            }
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}

/// Run a launcher command using a PTY (pseudo-terminal). Port of
/// `runLauncherWithPty`. Required for CLI tools (like opencode) that hang
/// when spawned with piped stdio.
///
/// Deviations from the Electron edition (node-pty):
/// - `portable-pty` does not expose the terminating signal of a naturally
///   reaped child, so a signal-killed child reports its raw exit code with
///   `signal: None` instead of `exit_code: None` + signal name.
/// - The node-pty lazy-load failure path does not exist (the PTY backend is
///   linked statically).
pub async fn run_launcher_with_pty<F>(
    input: &PtyRunInput,
    mut on_data: F,
) -> Result<LauncherRunResult, LauncherError>
where
    F: FnMut(String) + Send,
{
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let tokens = split_command(&input.command);
    let command = tokens.first().cloned().unwrap_or_default();
    let prefix_args = &tokens[tokens.len().min(1)..];

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 50,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| LauncherError::Pty(error.to_string()))?;

    let mut cmd = CommandBuilder::new(&command);
    cmd.args(prefix_args);
    cmd.args(&input.args);
    cmd.cwd(&input.cwd);
    // TS: `env: mergeChildEnv(process.env, input.env)` (see run_launcher).
    let merged_env = merge_child_env(&std::env::vars().collect(), input.env.as_ref());
    cmd.env_clear();
    for (key, value) in merged_env {
        cmd.env(key, value);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|error| LauncherError::Pty(error.to_string()))?;
    // Drop the slave side so the master sees EOF when the child exits.
    drop(pair.slave);
    let pid = child.process_id();
    let mut killer = child.clone_killer();

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| LauncherError::Pty(error.to_string()))?;
    // Keep the writer alive for the duration of the run (node-pty keeps the
    // child's stdin open; dropping the writer would send EOF).
    let _writer = pair
        .master
        .take_writer()
        .map_err(|error| LauncherError::Pty(error.to_string()))?;

    // The master reader blocks, so pump it on a dedicated thread and forward
    // chunks over a channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut wait = tokio::task::spawn_blocking(move || child.wait());
    let timeout = tokio::time::sleep(std::time::Duration::from_millis(input.timeout_ms));
    tokio::pin!(timeout);
    let abort = wait_for_abort(input.abort.clone());
    tokio::pin!(abort);

    // TS `onAbort`: SIGTERM, then SIGKILL 1.5s later if the child still has
    // not exited (CLIs may trap SIGTERM). The delayed kill is a no-op when
    // the process already exited (ESRCH).
    let kill_with_escalation = |pid: Option<u32>, killer: &mut Box<dyn portable_pty::ChildKiller + Send + Sync>| {
        send_sigterm(pid);
        if let Some(pid) = pid {
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                send_sigkill(Some(pid));
            });
        } else {
            let _ = killer.kill();
        }
    };

    let mut drained = false;
    let mut decoder = Utf8StreamDecoder::default();
    let result = loop {
        tokio::select! {
            status = &mut wait => {
                let status = status
                    .map_err(|error| LauncherError::Pty(format!("wait task failed: {error}")))?
                    .map_err(LauncherError::Io)?;
                break LauncherRunResult {
                    exit_code: Some(status.exit_code() as i32),
                    signal: None,
                    timed_out: false,
                };
            }
            _ = &mut timeout => {
                // Mirror node-pty: SIGTERM on timeout, resolve immediately
                // with a null exit code and the numeric signal as a string.
                // The detached wait task still reaps the child afterwards.
                kill_with_escalation(pid, &mut killer);
                break LauncherRunResult {
                    exit_code: None,
                    signal: Some("15".to_string()),
                    // The deadline fired (not a user abort or external signal).
                    timed_out: input
                        .abort
                        .as_ref()
                        .map(|flag| !flag.load(Ordering::SeqCst))
                        .unwrap_or(true),
                };
            }
            // Abort: same shape as the timeout arm (TS `onAbort` kills with
            // SIGTERM and lets the run resolve; the caller checks the flag).
            _ = &mut abort => {
                kill_with_escalation(pid, &mut killer);
                break LauncherRunResult {
                    exit_code: None,
                    signal: Some("15".to_string()),
                    timed_out: false,
                };
            }
            chunk = rx.recv() => {
                match chunk {
                    Some(bytes) => {
                        let cleaned = clean_pty_output(&decoder.push(&bytes));
                        if !cleaned.is_empty() {
                            on_data(cleaned);
                        }
                    }
                    None => {
                        // Reader thread ended (EOF); keep waiting for exit.
                        // Avoid busy-looping on a closed channel.
                        if drained {
                            futures_lite_park().await;
                        }
                        drained = true;
                    }
                }
            }
        }
    };

    // Best-effort drain of any output buffered between the last recv and exit.
    while let Ok(bytes) = rx.try_recv() {
        let cleaned = clean_pty_output(&decoder.push(&bytes));
        if !cleaned.is_empty() {
            on_data(cleaned);
        }
    }
    let tail = clean_pty_output(&decoder.finish());
    if !tail.is_empty() {
        on_data(tail);
    }

    Ok(result)
}

/// Once the reader channel is closed there is nothing left to poll; yield to
/// the runtime so the closed-channel select arm does not spin.
async fn futures_lite_park() {
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
}

fn command_not_found_error(command: &str) -> LauncherError {
    let message = match install_hint_for(command) {
        Some(hint) => format!("Command '{command}' not found. Install with: {hint}"),
        None => format!("Command '{command}' not found in PATH. Please install it and try again."),
    };
    LauncherError::NotFound(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(actor: &str, command: &str, prompt_file: &str) -> LauncherCommandInput {
        LauncherCommandInput {
            actor: actor.to_string(),
            command: command.to_string(),
            prompt_file: prompt_file.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn builds_claude_non_interactive_stream_json_command() {
        let mut i = input("claude", "claude --dangerously-skip-permissions", "/tmp/prompt.md");
        i.prompt_text = Some("hello".to_string());
        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "claude".to_string(),
                args: vec![
                    "--dangerously-skip-permissions",
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--input-format",
                    "text"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                env: None,
                kind: LauncherCommandKind::NativeClaude,
                stdin_text: Some("hello".to_string()),
            }
        );
    }

    #[test]
    fn builds_codex_exec_json_command() {
        let mut i = input("codex", "codex", "/tmp/prompt.md");
        i.prompt_text = Some("hello".to_string());
        i.output_file = Some("/tmp/output.md".to_string());
        i.repo_root = Some("/tmp/repo".to_string());
        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "codex".to_string(),
                args: vec![
                    "exec",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--json",
                    "--skip-git-repo-check",
                    "-C",
                    "/tmp/repo",
                    "-o",
                    "/tmp/output.md",
                    "-"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                env: None,
                kind: LauncherCommandKind::NativeCodex,
                stdin_text: Some("hello".to_string()),
            }
        );
    }

    #[test]
    fn builds_codex_exec_resume_command_after_exec_options() {
        let mut i = input("codex", "codex --profile native --full-auto", "/tmp/prompt.md");
        i.prompt_text = Some("hello".to_string());
        i.output_file = Some("/tmp/output.md".to_string());
        i.repo_root = Some("/tmp/repo".to_string());
        i.session_id = Some("codex-thread".to_string());
        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "codex".to_string(),
                args: vec![
                    "--profile",
                    "native",
                    "exec",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--json",
                    "--skip-git-repo-check",
                    "-C",
                    "/tmp/repo",
                    "-o",
                    "/tmp/output.md",
                    "resume",
                    "codex-thread",
                    "-"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                env: None,
                kind: LauncherCommandKind::NativeCodex,
                stdin_text: Some("hello".to_string()),
            }
        );
    }

    #[test]
    fn builds_cursor_cli_stream_json_command_with_partial_text_deltas() {
        let mut i = input("cursor", "cursor-agent --model gpt-5", "/tmp/prompt.md");
        i.prompt_text = Some("hello from prompt".to_string());
        i.session_id = Some("cursor-chat".to_string());
        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "cursor-agent".to_string(),
                args: vec![
                    "--model",
                    "gpt-5",
                    "--print",
                    "--force",
                    "--output-format",
                    "stream-json",
                    "--stream-partial-output",
                    "--single-turn",
                    "--resume",
                    "cursor-chat",
                    "hello from prompt"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                env: None,
                kind: LauncherCommandKind::NativeCursor,
                stdin_text: None,
            }
        );
    }

    #[test]
    fn recognizes_both_cursor_cli_executable_names() {
        assert_eq!(
            command_kind_for("cursor", "cursor-agent"),
            LauncherCommandKind::NativeCursor
        );
        assert_eq!(
            command_kind_for("cursor", "agent"),
            LauncherCommandKind::NativeCursor
        );
    }

    #[test]
    fn builds_agy_stream_json_stdin_command_with_print_timeout_and_conversation() {
        let mut i = input("agy", "agy", "/tmp/prompt.md");
        i.prompt_text = Some("hello from agy".to_string());
        i.session_id = Some("agy-conversation".to_string());
        i.timeout_seconds = Some(7200);
        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "agy".to_string(),
                args: vec![
                    "--output-format",
                    "stream-json",
                    "--input-format",
                    "stream-json",
                    "--dangerously-skip-permissions",
                    "--print-timeout=7200s",
                    "--conversation",
                    "agy-conversation",
                    "-p="
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                env: None,
                kind: LauncherCommandKind::NativeAgy,
                stdin_text: Some(
                    "{\"event\":\"user\",\"message\":{\"content\":\"hello from agy\"}}\n".to_string()
                ),
            }
        );
    }

    #[test]
    fn agy_print_timeout_defaults_to_two_hours_and_floors_at_one_second() {
        let mut i = input("agy", "agy", "/tmp/prompt.md");
        i.prompt_text = Some("hi".to_string());
        let command = build_launcher_command(&i);
        assert!(command.args.contains(&"--print-timeout=7200s".to_string()));

        i.timeout_seconds = Some(0);
        let command = build_launcher_command(&i);
        assert!(command.args.contains(&"--print-timeout=1s".to_string()));
    }

    #[test]
    fn recognizes_agy_executable_and_actor_fallback() {
        assert_eq!(
            command_kind_for("agy", "agy"),
            LauncherCommandKind::NativeAgy
        );
        assert_eq!(
            command_kind_for("agy", ""),
            LauncherCommandKind::NativeAgy
        );
        assert_eq!(
            command_kind_for("agy", "antigravity"),
            LauncherCommandKind::NativeAgy
        );
        // Unknown wrapper basename must still use native_agy — never contract --actor
        assert_eq!(
            command_kind_for("agy", "my-agy-wrapper"),
            LauncherCommandKind::NativeAgy
        );
        let mut i = input("agy", "my-agy-wrapper", "/tmp/prompt.md");
        i.prompt_text = Some("hi".to_string());
        i.timeout_seconds = Some(120);
        assert!(!build_launcher_command(&i).args.contains(&"--actor".to_string()));
    }

    #[test]
    fn builds_opencode_json_run_command_with_prompt_as_positional_argument() {
        let mut i = input("opencode", "opencode", "/tmp/prompt.md");
        i.prompt_text = Some("hello from prompt".to_string());
        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "opencode".to_string(),
                args: vec![
                    "run",
                    "--format",
                    "json",
                    "--dangerously-skip-permissions",
                    "hello from prompt"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                env: None,
                kind: LauncherCommandKind::NativeOpencode,
                stdin_text: None,
            }
        );
    }

    #[test]
    fn builds_opencode_resume_command_with_session_before_prompt() {
        let mut i = input("opencode", "opencode", "/tmp/prompt.md");
        i.prompt_text = Some("hello from prompt".to_string());
        i.session_id = Some("opencode-session".to_string());
        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "opencode".to_string(),
                args: vec![
                    "run",
                    "--format",
                    "json",
                    "--dangerously-skip-permissions",
                    "--session",
                    "opencode-session",
                    "hello from prompt"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                env: None,
                kind: LauncherCommandKind::NativeOpencode,
                stdin_text: None,
            }
        );
    }

    #[test]
    fn builds_kimi_code_stream_json_command_with_p_prompt() {
        let mut i = input("kimi", "kimi", "/tmp/prompt.md");
        i.prompt_text = Some("hello from prompt".to_string());
        i.session_id = Some("kimi-session".to_string());
        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "kimi".to_string(),
                args: vec![
                    "-p",
                    "hello from prompt",
                    "--output-format",
                    "stream-json",
                    "-S",
                    "kimi-session"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                env: None,
                kind: LauncherCommandKind::NativeKimi,
                stdin_text: None,
            }
        );
    }

    #[test]
    fn builds_kimi_code_command_without_session_when_no_session_id() {
        let mut i = input("kimi", "kimi", "/tmp/prompt.md");
        i.prompt_text = Some("hello from prompt".to_string());
        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "kimi".to_string(),
                args: vec!["-p", "hello from prompt", "--output-format", "stream-json"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                env: None,
                kind: LauncherCommandKind::NativeKimi,
                stdin_text: None,
            }
        );
    }

    #[test]
    fn builds_custom_launcher_contract_flags_and_environment() {
        let mut i = input("claude", "/tmp/run-actor --flag", "/tmp/prompt.md");
        i.mode = Some("resume".to_string());
        i.repo_root = Some("/tmp/repo".to_string());
        i.task_dir = Some("/tmp/task".to_string());
        i.run_id = Some("run-1".to_string());
        i.output_file = Some("/tmp/output.md".to_string());
        i.event_file = Some("/tmp/events.jsonl".to_string());
        i.session_id = Some("claude-session".to_string());

        let expected_env: HashMap<String, String> = [
            ("BUDDY_ACTOR", "claude"),
            ("BUDDY_MODE", "resume"),
            ("BUDDY_REPO_ROOT", "/tmp/repo"),
            ("BUDDY_TASK_DIR", "/tmp/task"),
            ("BUDDY_RUN_ID", "run-1"),
            ("BUDDY_PROMPT_FILE", "/tmp/prompt.md"),
            ("BUDDY_OUTPUT_FILE", "/tmp/output.md"),
            ("BUDDY_EVENT_FILE", "/tmp/events.jsonl"),
            ("BUDDY_SESSION_ID", "claude-session"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        assert_eq!(
            build_launcher_command(&i),
            LauncherCommand {
                command: "/tmp/run-actor".to_string(),
                args: vec![
                    "--flag",
                    "--actor",
                    "claude",
                    "--mode",
                    "resume",
                    "--repo-root",
                    "/tmp/repo",
                    "--task-dir",
                    "/tmp/task",
                    "--run-id",
                    "run-1",
                    "--prompt-file",
                    "/tmp/prompt.md",
                    "--output-file",
                    "/tmp/output.md",
                    "--event-file",
                    "/tmp/events.jsonl",
                    "--session-id",
                    "claude-session"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                env: Some(expected_env),
                kind: LauncherCommandKind::Contract,
                stdin_text: None,
            }
        );
    }

    // --- Extra coverage for the public helpers not exercised by the TS tests.

    #[test]
    fn split_command_keeps_quoted_segments() {
        assert_eq!(
            split_command("opencode -m \"provider/kimi k2\""),
            vec!["opencode", "-m", "provider/kimi k2"]
        );
        assert_eq!(split_command(""), vec![""]);
    }

    #[test]
    fn kind_needs_pty_only_for_opencode() {
        assert!(kind_needs_pty(LauncherCommandKind::NativeOpencode));
        assert!(!kind_needs_pty(LauncherCommandKind::NativeClaude));
        assert!(!kind_needs_pty(LauncherCommandKind::Contract));
    }

    #[test]
    fn parser_actor_follows_kind() {
        assert_eq!(
            parser_actor_for_kind("kimi", LauncherCommandKind::NativeOpencode),
            "opencode"
        );
        assert_eq!(
            parser_actor_for_kind("kimi", LauncherCommandKind::NativeKimi),
            "kimi"
        );
        assert_eq!(
            parser_actor_for_kind("custom", LauncherCommandKind::Contract),
            "custom"
        );
    }

    #[test]
    fn wecode_detection_mirrors_command_kind_for() {
        assert!(is_wecode_command("/usr/local/bin/wecode --flag"));
        assert!(is_wecode_claude_command("wecode"));
        assert!(is_wecode_claude_command("wecode --dangerously-skip-permissions"));
        assert!(!is_wecode_claude_command("wecode codex"));
        assert!(is_wecode_codex_command("wecode codex --profile native"));
        assert!(!is_wecode_codex_command("wecode"));
        assert_eq!(
            command_kind_for("claude", "wecode"),
            LauncherCommandKind::NativeClaude
        );
        assert_eq!(
            command_kind_for("codex", "wecode codex"),
            LauncherCommandKind::NativeCodex
        );
        // Empty command falls back to actor inference.
        assert_eq!(
            command_kind_for("claude", ""),
            LauncherCommandKind::NativeClaude
        );
        assert_eq!(
            command_kind_for("custom", "/tmp/run-actor"),
            LauncherCommandKind::Contract
        );
    }

    #[tokio::test]
    async fn run_launcher_pipes_stdin_to_stdout() {
        // `cat` echoes stdin to stdout; exercises spawn, stdin write, line
        // callbacks and exit-code reporting without external CLIs.
        let input = RunLauncherInput {
            command: "cat".to_string(),
            cwd: "/tmp".to_string(),
            stdin_text: Some("hello\nworld\n".to_string()),
            timeout_ms: 10_000,
            ..Default::default()
        };
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();
        let result = run_launcher(
            &input,
            |line| stdout_lines.push(line),
            |line| stderr_lines.push(line),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            LauncherRunResult {
                exit_code: Some(0),
                signal: None,
                timed_out: false,
            }
        );
        assert_eq!(stdout_lines, vec!["hello", "world"]);
        assert!(stderr_lines.is_empty());
    }

    #[tokio::test]
    async fn run_launcher_reports_command_not_found() {
        let input = RunLauncherInput {
            command: "definitely-not-a-real-buddy-command".to_string(),
            cwd: "/tmp".to_string(),
            timeout_ms: 1_000,
            ..Default::default()
        };
        let error = run_launcher(&input, |_| {}, |_| {}).await.unwrap_err();
        match error {
            LauncherError::NotFound(message) => {
                assert!(message.contains("definitely-not-a-real-buddy-command"));
                assert!(message.contains("not found in PATH"));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_launcher_with_pty_captures_output() {
        let input = PtyRunInput {
            command: "echo".to_string(),
            args: vec!["hello-pty".to_string()],
            cwd: "/tmp".to_string(),
            timeout_ms: 10_000,
            ..Default::default()
        };
        let mut data = String::new();
        let result = run_launcher_with_pty(&input, |chunk| data.push_str(&chunk))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(data.contains("hello-pty"), "expected pty output, got {data:?}");
    }

    /// Manual reproduction against the real opencode CLI — run with:
    /// `cargo test --lib real_opencode_pty -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn real_opencode_pty_completes() {
        let input = PtyRunInput {
            command: "opencode".to_string(),
            args: vec![
                "run".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "只回复：好".to_string(),
            ],
            cwd: "/tmp".to_string(),
            timeout_ms: 120_000,
            ..Default::default()
        };
        let mut data = String::new();
        let result = run_launcher_with_pty(&input, |chunk| data.push_str(&chunk))
            .await
            .unwrap();
        eprintln!("exit={:?} signal={:?} bytes={}", result.exit_code, result.signal, data.len());
        assert_eq!(result.exit_code, Some(0), "opencode should exit 0; got {result:?}");
        assert!(data.contains("step_start") || data.contains("text"), "expected json events, got: {}", &data[..data.len().min(500)]);
    }

    #[test]
    fn utf8_stream_decoder_keeps_split_multibyte_chars_intact() {
        let text = "中文输出，emoji 😀，混合 ASCII。";
        let bytes = text.as_bytes();
        // Every possible split point must decode losslessly.
        for cut in 0..bytes.len() {
            let mut decoder = Utf8StreamDecoder::default();
            let mut out = decoder.push(&bytes[..cut]);
            out.push_str(&decoder.push(&bytes[cut..]));
            out.push_str(&decoder.finish());
            assert_eq!(out, text, "split at byte {cut}");
        }
    }

    #[test]
    fn utf8_stream_decoder_replaces_invalid_bytes() {
        let mut decoder = Utf8StreamDecoder::default();
        let out = decoder.push(b"ok\xff\xfe done");
        assert_eq!(out, "ok\u{FFFD}\u{FFFD} done");
        assert_eq!(decoder.finish(), "");
    }

    #[test]
    fn utf8_stream_decoder_flush_replaces_incomplete_tail() {
        // Half of 中 (E4 B8 AD) left at EOF is invalid input.
        let mut decoder = Utf8StreamDecoder::default();
        assert_eq!(decoder.push(&[0xE4, 0xB8]), "");
        assert_eq!(decoder.finish(), "\u{FFFD}");
    }

    #[test]
    fn utf8_stream_decoder_recovers_when_partial_is_not_completed() {
        // A buffered partial sequence followed by a non-continuation byte:
        // the stale bytes are replaced once and the new byte decodes normally.
        let mut decoder = Utf8StreamDecoder::default();
        assert_eq!(decoder.push(&[0xE4, 0xB8]), "");
        assert_eq!(decoder.push(b"a"), "\u{FFFD}a");
    }

    #[tokio::test]
    async fn run_launcher_preserves_cjk_split_across_read_boundary() {
        // ~1000 lines × 23 bytes >> the 8192-byte read buffer, so reads split
        // multi-byte UTF-8 sequences; before the streaming decoder this
        // corrupted actor output into `��`.
        let input = RunLauncherInput {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "i=0; while [ $i -lt 1000 ]; do printf '测试输出中文内容😀\\n'; i=$((i+1)); done".to_string(),
            ],
            cwd: "/tmp".to_string(),
            timeout_ms: 10_000,
            ..Default::default()
        };
        let mut out = String::new();
        run_launcher(
            &input,
            |line| out.push_str(&line),
            |_| {},
        )
        .await
        .unwrap();
        assert!(
            !out.contains('\u{FFFD}'),
            "mojibake in output: {}",
            &out[..out.len().min(200)]
        );
        assert!(out.contains("测试输出中文内容😀"));
    }

    // -----------------------------------------------------------------------
    // Stream draining / deadlines (upstream buddy-launchers.test.ts +
    // buddy-launcher-timeout.test.ts)
    // -----------------------------------------------------------------------

    #[test]
    fn stream_drain_ms_bounds() {
        assert_eq!(stream_drain_ms(0), 250);
        assert_eq!(stream_drain_ms(10), 50);
        assert_eq!(stream_drain_ms(50), 50);
        assert_eq!(stream_drain_ms(200), 200);
        assert_eq!(stream_drain_ms(600_000), 500);
    }

    #[test]
    fn line_splitter_keeps_partial_lines_across_chunks() {
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut splitter = LineSplitter::new({
            let lines = lines.clone();
            move |line| lines.lock().unwrap().push(line)
        });
        splitter.push("{\"type\":\"ass");
        splitter.push("istant\"}\nsec");
        splitter.push("ond\r\n");
        assert_eq!(
            *lines.lock().unwrap(),
            vec!["{\"type\":\"assistant\"}", "second"]
        );
        splitter.push("tail-without-newline");
        assert_eq!(lines.lock().unwrap().len(), 2);
        splitter.flush();
        assert_eq!(lines.lock().unwrap()[2], "tail-without-newline");
    }

    #[tokio::test]
    async fn run_launcher_reassembles_ndjson_split_across_chunks() {
        let input = RunLauncherInput {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf '%s' '{\"a\":'; sleep 0.2; printf '%s\\n' '1}'".to_string(),
            ],
            cwd: "/tmp".to_string(),
            timeout_ms: 10_000,
            ..Default::default()
        };
        let mut lines = Vec::new();
        run_launcher(&input, |line| lines.push(line), |_| {})
            .await
            .unwrap();
        assert_eq!(lines, vec!["{\"a\":1}"]);
    }

    #[tokio::test]
    async fn run_launcher_returns_when_grandchild_keeps_pipe_open() {
        // The grandchild (`sleep`) inherits stdout and keeps the pipe open
        // after the launcher exits — the drain must be bounded.
        let input = RunLauncherInput {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 3 & echo done".to_string()],
            cwd: "/tmp".to_string(),
            timeout_ms: 30_000,
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let mut lines = Vec::new();
        let result = run_launcher(&input, |line| lines.push(line), |_| {})
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        assert_eq!(lines, vec!["done"]);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "drain was not bounded: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn run_launcher_marks_deadline_as_timed_out() {
        let input = RunLauncherInput {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 10".to_string()],
            cwd: "/tmp".to_string(),
            timeout_ms: 300,
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let result = run_launcher(&input, |_| {}, |_| {}).await.unwrap();
        assert!(result.timed_out);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "timeout did not settle: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn run_launcher_force_kills_when_sigterm_is_ignored() {
        let input = RunLauncherInput {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "trap '' TERM; sleep 5".to_string()],
            cwd: "/tmp".to_string(),
            timeout_ms: 300,
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let result = run_launcher(&input, |_| {}, |_| {}).await.unwrap();
        assert!(result.timed_out);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(1500),
            "SIGKILL escalation fired too early: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "SIGKILL escalation never settled: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn run_launcher_abort_is_not_reported_as_timed_out() {
        let abort = Arc::new(AtomicBool::new(false));
        let input = RunLauncherInput {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 10".to_string()],
            cwd: "/tmp".to_string(),
            timeout_ms: 300,
            abort: Some(abort.clone()),
            ..Default::default()
        };
        abort.store(true, Ordering::SeqCst);
        let result = run_launcher(&input, |_| {}, |_| {}).await.unwrap();
        assert!(!result.timed_out);
    }
}
