//! In-process Claude / Codex CLI streaming. Spawns the provider CLI directly
//! (no localhost HTTP proxy), feeds it the chat history over stdin, and forwards
//! each decoded text chunk to a caller-supplied callback. The child is spawned
//! with `kill_on_drop` so aborting the owning task terminates the CLI process.

use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
#[cfg(windows)]
use std::os::windows::process::CommandExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_stream::wrappers::LinesStream;

use super::ChatMessage;
use crate::companion_config::SessionLimits;
use crate::notebook::{self, NotebookContext, NotebookReply};

/// Default Claude model when the user has not configured one. Codex uses the
/// model from its own CLI configuration and existing `ChatGPT` login.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-haiku-4-5";

/// Name of the Codex working directory (used as both the WSL `/tmp/<name>` path
/// and the Windows `temp_dir().join(<name>)` path).
const CODEX_WORKDIR: &str = "aigc-codex-workdir";
/// Optional user-maintained, read-only knowledge base for the native Codex CLI.
/// An explicit environment variable wins; the Desktop folder is a convenient
/// default for this companion without granting broad filesystem access.
const CODEX_REFERENCE_DIR_ENV: &str = "AIGC_REFERENCE_DIR";
const DEFAULT_CODEX_REFERENCE_DIR: &str = "AI DOCS";

// Keep this override local to companion invocations. The built-in `openai`
// provider cannot be overridden, and the old WebSocket feature flags are removed.
// HTTPS streaming avoids the observed Windows WebSocket resets while preserving
// the same ChatGPT backend and login. One stream retry, no nested HTTP retries.
const CODEX_HTTPS_PROVIDER: &str = concat!(
    "model_providers.companion_https={",
    "name=\"OpenAI HTTPS\",",
    "base_url=\"https://chatgpt.com/backend-api/codex\",",
    "wire_api=\"responses\",requires_openai_auth=true,supports_websockets=false,",
    "request_max_retries=0,stream_max_retries=1}",
);

/// Successful CLI session owned by one overlay conversation. A fingerprint of
/// the expected history prevents resuming after edits, provider switches, or
/// cancellation; it avoids keeping a second full chat history in memory.
#[derive(Debug)]
pub struct CodexSession {
    thread_id: String,
    conversation_id: u64,
    mode: CliMode,
    workdir: String,
    system_prompt: String,
    notebook_identity: String,
    expected_messages: usize,
    expected_history: u64,
    turns: usize,
    image_turns: usize,
}

impl CodexSession {
    fn can_resume(
        &self,
        cfg: &CliConfig,
        system_prompt: &str,
        messages: &[ChatMessage],
        conversation_id: u64,
        limits: &SessionLimits,
        notebook_identity: &str,
    ) -> bool {
        self.conversation_id == conversation_id
            && self.mode == cfg.codex
            && self.workdir == cfg.codex_workdir
            && self.system_prompt == system_prompt
            && self.notebook_identity == notebook_identity
            && self.turns < limits.max_turns
            && self.image_turns < limits.max_image_turns
            && messages.len() == self.expected_messages + 1
            && messages
                .last()
                .is_some_and(|message| message.role == "user")
            && history_fingerprint(&messages[..self.expected_messages], None)
                == self.expected_history
    }
}

fn history_fingerprint(messages: &[ChatMessage], reply: Option<&str>) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (messages.len() + usize::from(reply.is_some())).hash(&mut hash);
    for message in messages {
        message.role.hash(&mut hash);
        message.content.hash(&mut hash);
    }
    if let Some(reply) = reply {
        "assistant".hash(&mut hash);
        reply.hash(&mut hash);
    }
    hash.finish()
}

/// The CLI reads an image file rather than base64 on stdin. Keep the PNG alive
/// until the child has exited and remove it on errors or task cancellation too.
struct CodexFile(PathBuf);

impl CodexFile {
    fn from_base64(data: &str) -> Result<Self, String> {
        // Bound malformed input before allocating its decoded buffer.
        if data.len() > 48 * 1024 * 1024 {
            return Err("Screenshot is too large for the Codex CLI.".to_owned());
        }
        let png = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("Invalid screenshot base64: {e}"))?;
        if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err("Codex screenshot is not a PNG.".to_owned());
        }
        Self::create(&png, "png")
    }

    fn create(data: &[u8], extension: &str) -> Result<Self, String> {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| format!("Cannot name Codex input file: {e}"))?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aigc-codex-{}-{stamp}-{}.{extension}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed),
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("Cannot create Codex input file: {e}"))?;
        let image = Self(path);
        let result = file.write_all(data);
        drop(file);
        result.map_err(|e| format!("Cannot write Codex input file: {e}"))?;
        Ok(image)
    }

    async fn cli_path(&self, mode: CliMode) -> Result<String, String> {
        if mode != CliMode::Wsl {
            return Ok(self.0.to_string_lossy().into_owned());
        }
        // Ask the actual distro about its mount layout. --exec avoids a shell;
        // the Windows path remains one argument even with spaces/apostrophes.
        let mut cmd = Command::new("wsl.exe");
        cmd.args(["--exec", "wslpath", "-a", "-u"])
            .arg(&self.0)
            .kill_on_drop(true);
        no_window(&mut cmd);
        let output = cmd
            .output()
            .await
            .map_err(|e| format!("Cannot convert Codex file path using WSL: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "WSL Codex file path conversion failed: {}",
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
        let path = String::from_utf8(output.stdout)
            .map_err(|_| "WSL returned a non-UTF-8 Codex file path.".to_owned())?;
        let path = path.trim_end_matches(['\r', '\n']);
        if !path.starts_with('/') || path.contains(['\r', '\n']) {
            return Err("WSL returned an invalid Codex file path.".to_owned());
        }
        Ok(path.to_owned())
    }
}

impl Drop for CodexFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            tracing::warn!("Could not remove temporary Codex input file: {error}");
        }
    }
}

/// Windows `CREATE_NO_WINDOW` flag -- prevents console popups from `wsl.exe` and
/// other console-subsystem processes.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How to invoke a CLI tool.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    /// Not available on this system.
    #[default]
    Unavailable,
    /// Available directly on the Windows PATH.
    Native,
    /// Available inside WSL (invoke via `wsl.exe`).
    Wsl,
}

impl CliMode {
    pub fn is_available(self) -> bool {
        !matches!(self, Self::Unavailable)
    }

    /// Human label for where the CLI was detected.
    pub fn location(self) -> &'static str {
        match self {
            Self::Native => "PATH",
            Self::Wsl => "WSL",
            Self::Unavailable => "",
        }
    }
}

/// Cached CLI availability, detected once at startup on a background thread.
#[derive(Debug, Clone, Default)]
pub struct CliConfig {
    pub claude: CliMode,
    pub codex: CliMode,
    pub codex_workdir: String,
}

/// Which content the parser decoded from one CLI stdout line.
#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    Text(String),
    Error(String),
}

/// Configure a `std::process::Command` to run silently (no console popup on
/// Windows, stdout/stderr discarded everywhere). Used for fire-and-forget
/// probes where we only care about the exit status.
fn silent(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Apply the Windows no-window flag to a tokio `Command`. No-op on non-Windows
/// so the launcher crate compiles for the Linux test runner.
#[allow(unused_variables, clippy::needless_pass_by_ref_mut)]
fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Escape a string for use inside a `bash -c` / `bash -ic` command.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Check if a CLI tool is available, first natively on the Windows PATH, then
/// inside WSL (using `bash -ic` so nvm / profile PATH is sourced).
pub fn detect_cli(name: &str) -> CliMode {
    let native = silent(std::process::Command::new(name).arg("--version"))
        .status()
        .is_ok_and(|status| status.success());
    if native {
        return CliMode::Native;
    }

    let version_cmd = format!("{name} --version");
    // A missing or broken WSL installation can take a long time to fail. Do not
    // let an unavailable Claude-in-WSL probe delay discovering native Codex.
    let mut command = std::process::Command::new("wsl.exe");
    command.args(["--", "bash", "-ic", &version_cmd]);
    let wsl = silent_status_with_timeout(&mut command, std::time::Duration::from_secs(2));
    if wsl {
        return CliMode::Wsl;
    }

    CliMode::Unavailable
}

/// Run a silent probe without allowing a broken optional dependency to hold up
/// provider discovery during app startup.
fn silent_status_with_timeout(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> bool {
    let Ok(mut child) = silent(cmd).spawn() else {
        return false;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Err(_) => return false,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
}

/// Use a deliberately chosen local reference directory when one exists. Codex is
/// still launched with its read-only sandbox, so this grants lookup capability
/// without allowing the companion to modify those files. Fall back to a small
/// temporary workspace when no reference directory is configured.
pub fn ensure_codex_workdir(mode: CliMode) -> String {
    if let CliMode::Wsl = mode {
        let dir = format!("/tmp/{CODEX_WORKDIR}");
        let _ = silent(std::process::Command::new("wsl.exe").args([
            "--",
            "bash",
            "-c",
            &format!("[ -d {dir}/.git ] || (mkdir -p {dir} && git -C {dir} init)"),
        ]))
        .status();
        return dir;
    }

    let configured = std::env::var_os(CODEX_REFERENCE_DIR_ENV).map(PathBuf::from);
    let desktop_default = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|home| home.join("Desktop").join(DEFAULT_CODEX_REFERENCE_DIR));
    if let Some(dir) = configured
        .into_iter()
        .chain(desktop_default)
        .find(|dir| dir.is_dir())
    {
        tracing::info!("Using Codex reference directory: {}", dir.display());
        return dir.to_string_lossy().into_owned();
    }

    let dir = std::env::temp_dir().join(CODEX_WORKDIR);
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = silent(
            std::process::Command::new("git")
                .args(["init"])
                .current_dir(&dir),
        )
        .status();
    }
    dir.to_string_lossy().into_owned()
}

/// Validate a model name: ASCII alphanumeric + hyphens, dots, underscores.
fn validate_model_name(model: &str) -> Result<(), String> {
    if model.is_empty() || model.len() > 128 {
        return Err("Invalid model name.".to_owned());
    }
    if !model
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
    {
        return Err("Invalid model name.".to_owned());
    }
    Ok(())
}

fn build_claude_input(messages: &[ChatMessage], screenshot: Option<&str>) -> String {
    // Collect all messages into a single user turn. Claude stream-json expects
    // one user message; conversation history is concatenated as text context.
    let mut combined_text = String::new();
    for msg in messages {
        if !combined_text.is_empty() {
            combined_text.push('\n');
        }
        let _ = write!(combined_text, "[{}]: {}", msg.role, msg.content);
    }

    let mut content_parts = vec![serde_json::json!({
        "type": "text",
        "text": combined_text,
    })];

    if let Some(data) = screenshot {
        content_parts.push(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": data,
            }
        }));
    }

    let input_msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": content_parts,
        },
        "parent_tool_use_id": null,
        "session_id": null,
    });

    let mut out = serde_json::to_string(&input_msg).unwrap_or_else(|e| {
        tracing::error!("Failed to serialize Claude input: {e}");
        String::new()
    });
    out.push('\n');
    out
}

fn build_codex_input(
    system_prompt: &str,
    messages: &[ChatMessage],
    limits: &SessionLimits,
) -> String {
    let mut text = String::new();
    if !system_prompt.is_empty() {
        text.push_str(system_prompt);
        text.push_str("\n\n");
    }
    let mut remaining = limits.handoff_chars;
    let mut recent = Vec::new();
    for (index, msg) in messages
        .iter()
        .rev()
        .take(limits.handoff_messages)
        .enumerate()
    {
        let prefix = format!("[{}]: ", msg.role);
        let overhead = prefix.chars().count() + 1;
        let current_question = index == 0 && msg.role == "user";
        if !current_question && remaining <= overhead {
            break;
        }
        let count = msg.content.chars().count();
        // A smaller history budget must never silently remove the beginning of
        // the user's current question (including its constraints/corrections).
        let keep = if current_question {
            count
        } else {
            count.min(remaining - overhead)
        };
        let content: String = msg.content.chars().skip(count - keep).collect();
        recent.push(format!("{prefix}{content}\n"));
        remaining = remaining.saturating_sub(overhead + keep);
        if keep < count {
            break;
        }
    }
    if recent.len() < messages.len() || remaining == 0 {
        text.push_str("[Earlier text omitted; recent conversation context follows.]\n");
    }
    for line in recent.iter().rev() {
        text.push_str(line);
    }
    text
}

/// Parse a single NDJSON line from Claude CLI stdout.
fn parse_claude_line(line: &str) -> Option<Parsed> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let msg_type = v.get("type")?.as_str()?;

    match msg_type {
        "stream_event" => {
            let delta_type = v
                .pointer("/event/delta/type")
                .and_then(serde_json::Value::as_str)?;
            if delta_type == "text_delta" {
                let text = v
                    .pointer("/event/delta/text")
                    .and_then(serde_json::Value::as_str)?;
                Some(Parsed::Text(text.to_owned()))
            } else {
                None
            }
        }
        "result" => {
            let is_error = v
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if is_error {
                let error_msg = v
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Unknown Claude CLI error");
                Some(Parsed::Error(error_msg.to_owned()))
            } else {
                // Successful result -- stream is complete.
                None
            }
        }
        // system, assistant, etc -- ignore.
        _ => None,
    }
}

/// State decoded from `codex exec --json`. Only assistant message items reach
/// the overlay; tool events and other protocol frames are never chat text.
#[derive(Default)]
struct CodexOutput {
    thread_id: Option<String>,
    completed: bool,
    last_error: Option<String>,
    answer: String,
    last_message: String,
    structured: bool,
}

impl CodexOutput {
    fn parse_line(&mut self, line: &str) -> Option<Parsed> {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                return Some(Parsed::Error(format!("Invalid Codex JSON output: {error}")))
            }
        };
        match v.get("type").and_then(serde_json::Value::as_str) {
            Some("thread.started") => {
                self.thread_id = v
                    .get("thread_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
                None
            }
            Some("item.completed")
                if v.pointer("/item/type").and_then(serde_json::Value::as_str)
                    == Some("agent_message") =>
            {
                let text = v
                    .pointer("/item/text")
                    .and_then(serde_json::Value::as_str)?;
                if text.is_empty() {
                    return None;
                }
                text.clone_into(&mut self.last_message);
                if self.structured {
                    // Commentary, schema envelopes and generated notes never
                    // enter the answer bubble or Speechify's selected text.
                    return None;
                }
                let chunk = if self.answer.is_empty() {
                    text.to_owned()
                } else {
                    format!("\n\n{text}")
                };
                self.answer.push_str(&chunk);
                Some(Parsed::Text(chunk))
            }
            Some("turn.completed") => {
                self.completed = true;
                None
            }
            Some("error") => {
                // Codex also emits retry notices as top-level `error` events.
                // Keep reading: only turn.failed, an unsuccessful exit, or an
                // incomplete turn is terminal. Do not kill an active reconnect
                // or send its diagnostic to chat, Speechify, or the notebook.
                let message = v
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Codex CLI reported an error.");
                tracing::warn!("Codex stream notice: {message}");
                self.last_error = Some(message.to_owned());
                None
            }
            Some("turn.failed") => {
                let message = v
                    .pointer("/error/message")
                    .or_else(|| v.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Codex CLI turn failed.");
                Some(Parsed::Error(message.to_owned()))
            }
            _ => None,
        }
    }

    fn finish(&self, expected_thread: Option<&str>) -> Result<&str, String> {
        if !self.completed {
            let mut message = "Codex CLI ended without completing the turn.".to_owned();
            if let Some(error) = &self.last_error {
                message.push_str(&format!(" Last error: {error}"));
            }
            return Err(message);
        }
        let thread = self
            .thread_id
            .as_deref()
            .ok_or_else(|| "Codex CLI did not report its session ID.".to_owned())?;
        if expected_thread.is_some_and(|expected| expected != thread) {
            return Err("Codex CLI resumed a different session; please retry.".to_owned());
        }
        if self.last_message.is_empty() {
            return Err("Codex CLI completed without an assistant response.".to_owned());
        }
        Ok(thread)
    }
}

/// One argv definition for native and WSL invocations. Resume image options
/// belong after `resume SESSION`. `-- -` prevents the fresh command's multi-
/// value --image option from consuming the stdin prompt marker as a filename.
fn codex_args(
    workdir: &str,
    thread: Option<&str>,
    image: Option<&str>,
    schema: Option<&str>,
) -> Vec<String> {
    // Enforce subscription authentication for this invocation without editing
    // the user's CLI configuration or selecting a different configured model.
    let mut args: Vec<String> = [
        "-a",
        "never",
        "-s",
        "read-only",
        "-c",
        "forced_login_method=\"chatgpt\"",
        "-c",
        "model_provider=\"companion_https\"",
        "-c",
        CODEX_HTTPS_PROVIDER,
        "-c",
        "features.unbounded_connection_retries=false",
        "-C",
        workdir,
        "exec",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(thread) = thread {
        args.extend(["resume".to_owned(), thread.to_owned()]);
    }
    args.extend(["--skip-git-repo-check".to_owned(), "--json".to_owned()]);
    if let Some(schema) = schema {
        args.extend(["--output-schema".to_owned(), schema.to_owned()]);
    }
    if let Some(image) = image {
        args.extend(["--image".to_owned(), image.to_owned()]);
    }
    args.extend(["--".to_owned(), "-".to_owned()]);
    args
}

fn codex_command(mode: CliMode, args: &[String]) -> Command {
    if mode == CliMode::Wsl {
        let invocation = std::iter::once("codex".to_owned())
            .chain(args.iter().map(|arg| shell_escape(arg)))
            .collect::<Vec<_>>()
            .join(" ");
        let mut command = Command::new("wsl.exe");
        command.args(["--", "bash", "-ic", &invocation]);
        command
    } else {
        let mut command = Command::new("codex");
        command.args(args);
        command
    }
}

/// Stream a Claude response by spawning the Claude CLI in stream-json mode.
pub async fn stream_claude<F>(
    cfg: &CliConfig,
    model: &str,
    system_prompt: &str,
    messages: &[ChatMessage],
    screenshot: Option<&str>,
    on_chunk: F,
) -> Result<(), String>
where
    F: FnMut(String) -> Result<(), String>,
{
    if !cfg.claude.is_available() {
        return Err("Claude CLI is not available on this system.".to_owned());
    }
    validate_model_name(model)?;

    let mut cmd = if let CliMode::Wsl = cfg.claude {
        let claude_args = format!(
            "claude -p --input-format stream-json --output-format stream-json \
             --verbose --include-partial-messages --tools '' \
             --no-session-persistence --model {} --system-prompt {}",
            shell_escape(model),
            shell_escape(system_prompt),
        );
        let mut c = Command::new("wsl.exe");
        c.args(["--", "bash", "-ic", &claude_args]);
        c
    } else {
        let mut c = Command::new("claude");
        c.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--tools",
            "",
            "--no-session-persistence",
            "--model",
            model,
            "--system-prompt",
            system_prompt,
        ]);
        c
    };

    let input = build_claude_input(messages, screenshot);
    run_cli(&mut cmd, input, on_chunk, parse_claude_line, "Claude").await
}

/// Stream a Codex response by spawning the Codex CLI in `exec` mode.
#[allow(clippy::too_many_arguments)] // Request inputs, limits, session, and streaming callback.
pub async fn stream_codex<F>(
    cfg: &CliConfig,
    system_prompt: &str,
    messages: &[ChatMessage],
    screenshot: Option<&str>,
    conversation_id: u64,
    session: &mut Option<CodexSession>,
    limits: &SessionLimits,
    notebook: Option<&NotebookContext>,
    mut on_chunk: F,
) -> Result<Option<NotebookReply>, String>
where
    F: FnMut(String) -> Result<(), String>,
{
    // Invalidate before any await or fallible operation. If this future is
    // cancelled, its uncertain CLI turn must never be resumed on the next call.
    let notebook_identity = notebook.map_or("", |context| context.identity.as_str());
    let previous = session.take().filter(|previous| {
        previous.can_resume(
            cfg,
            system_prompt,
            messages,
            conversation_id,
            limits,
            notebook_identity,
        )
    });
    if !cfg.codex.is_available() {
        return Err("Codex CLI is not available on this system.".to_owned());
    }

    let image = screenshot.map(CodexFile::from_base64).transpose()?;
    let image_path = match image.as_ref() {
        Some(image) => Some(image.cli_path(cfg.codex).await?),
        None => None,
    };
    let schema = notebook
        .map(|_| CodexFile::create(notebook::RESPONSE_SCHEMA.as_bytes(), "json"))
        .transpose()?;
    let schema_path = match schema.as_ref() {
        Some(schema) => Some(schema.cli_path(cfg.codex).await?),
        None => None,
    };
    let expected_thread = previous.as_ref().map(|session| session.thread_id.as_str());
    let args = codex_args(
        &cfg.codex_workdir,
        expected_thread,
        image_path.as_deref(),
        schema_path.as_deref(),
    );
    let mut cmd = codex_command(cfg.codex, &args);
    let mut input = if let Some(previous) = previous.as_ref() {
        build_codex_input("", &messages[previous.expected_messages..], limits)
    } else {
        build_codex_input(system_prompt, messages, limits)
    };
    if let Some(context) = notebook {
        // Notebook budgets are independent of the recent-chat handoff budget.
        // Include the latest snapshot on resumed turns as well as fresh ones.
        input = format!("{}{input}", context.prompt());
    }
    let mut output = CodexOutput {
        structured: notebook.is_some(),
        ..Default::default()
    };
    run_cli(
        &mut cmd,
        input,
        &mut on_chunk,
        |line| output.parse_line(line),
        "Codex",
    )
    .await?;
    let thread_id = output.finish(expected_thread)?.to_owned();
    let reply = if notebook.is_some() {
        let mut reply = notebook::decode_reply(&output.last_message)?;
        if let Some(request) = &reply.notebook_request {
            // Show only the app's question, never gameplay work or a model's
            // claim of approval before the user has chosen a destination.
            reply.answer = request.question();
        }
        on_chunk(reply.answer.clone())?;
        Some(reply)
    } else {
        None
    };
    let answer = reply
        .as_ref()
        .map_or(output.answer.as_str(), |reply| reply.answer.as_str());
    *session = Some(CodexSession {
        thread_id,
        conversation_id,
        mode: cfg.codex,
        workdir: cfg.codex_workdir.clone(),
        system_prompt: system_prompt.to_owned(),
        notebook_identity: notebook_identity.to_owned(),
        expected_messages: messages.len() + 1,
        expected_history: history_fingerprint(messages, Some(answer)),
        turns: previous.as_ref().map_or(1, |session| session.turns + 1),
        image_turns: previous.as_ref().map_or(0, |session| session.image_turns)
            + usize::from(screenshot.is_some()),
    });
    Ok(reply)
}

/// Spawn a CLI child, write `input` to stdin, and stream parsed stdout lines to
/// `on_chunk`. stdin/stdout/stderr are driven concurrently in this one future so
/// that aborting the owning task drops the child; `kill_on_drop` then terminates
/// it. Note: in WSL mode the direct child is `wsl.exe`, so this ends the relay
/// but may orphan the in-distro CLI process (a known limitation, same as before).
async fn run_cli<F, P>(
    cmd: &mut Command,
    input: String,
    mut on_chunk: F,
    mut parse_line: P,
    label: &str,
) -> Result<(), String>
where
    F: FnMut(String) -> Result<(), String>,
    P: FnMut(&str) -> Option<Parsed>,
{
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    no_window(cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn {label} CLI: {e}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("Failed to open {label} stdin."))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("Failed to open {label} stdout."))?;
    let stderr = child.stderr.take();

    let write_fut = async move {
        stdin
            .write_all(input.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to {label} CLI: {e}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| format!("Failed to close {label} CLI stdin: {e}"))?;
        // Dropping stdin closes the pipe so the CLI knows the input is complete.
        Ok::<_, String>(())
    };

    let stderr_fut = async move {
        let mut tail = String::new();
        if let Some(stderr) = stderr {
            let reader = BufReader::new(stderr);
            let mut lines = LinesStream::new(reader.lines());
            while let Some(line) = lines.next().await {
                let line = line.map_err(|e| format!("Failed to read {label} CLI stderr: {e}"))?;
                if !line.trim().is_empty() {
                    tracing::warn!("{label} stderr: {line}");
                    tail.push_str(&line);
                    tail.push('\n');
                    if tail.len() > 8192 {
                        let mut start = tail.len() - 8192;
                        while !tail.is_char_boundary(start) {
                            start += 1;
                        }
                        tail.drain(..start);
                    }
                }
            }
        }
        Ok::<_, String>(tail)
    };

    let read_fut = async {
        let reader = BufReader::new(stdout);
        let mut lines = LinesStream::new(reader.lines());
        while let Some(item) = lines.next().await {
            let line = item.map_err(|e| format!("Failed to read from {label} CLI: {e}"))?;
            if line.trim().is_empty() {
                continue;
            }
            match parse_line(&line) {
                Some(Parsed::Text(text)) => on_chunk(text)?,
                Some(Parsed::Error(message)) => return Err(message),
                None => {}
            }
        }
        Ok(())
    };

    let stderr_tail = match tokio::try_join!(write_fut, stderr_fut, read_fut) {
        Ok(((), tail, ())) => tail,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed to wait for {label} CLI: {e}"))?;
    if !status.success() {
        return Err(format!(
            "{label} CLI exited with {status}: {}",
            stderr_tail.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    // ---------------- shell_escape ----------------

    #[test]
    fn shell_escape_wraps_in_single_quotes() {
        assert_eq!(shell_escape("plain"), "'plain'");
    }

    #[test]
    fn shell_escape_preserves_spaces_and_special_chars() {
        assert_eq!(shell_escape("a b $c & d"), "'a b $c & d'");
    }

    #[test]
    fn shell_escape_escapes_inner_single_quote() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_escape_handles_empty_string() {
        assert_eq!(shell_escape(""), "''");
    }

    // ---------------- validate_model_name ----------------

    #[test]
    fn validate_model_name_accepts_typical_ids() {
        for ok in [
            "gemini-2.5-flash",
            "claude-haiku-4-5",
            "gpt-4o",
            "model_v2",
            "Some.Model.With.Dots",
            "a",
        ] {
            assert!(validate_model_name(ok).is_ok(), "should accept: {ok}");
        }
    }

    #[test]
    fn validate_model_name_rejects_empty_and_oversize() {
        assert!(validate_model_name("").is_err());
        let oversize = "a".repeat(129);
        assert!(validate_model_name(&oversize).is_err());
    }

    #[test]
    fn validate_model_name_rejects_path_traversal() {
        for bad in [
            "../foo", "foo/bar", "foo\\bar", "foo bar", "foo:bar", "foo$",
        ] {
            assert!(validate_model_name(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn validate_model_name_rejects_non_ascii() {
        assert!(validate_model_name("mod\u{e8}le").is_err());
    }

    // ---------------- build_codex_input ----------------

    #[test]
    fn codex_input_omits_system_prompt_when_empty() {
        let out = build_codex_input("", &[msg("user", "hello")], &SessionLimits::default());
        assert_eq!(out, "[user]: hello\n");
    }

    #[test]
    fn codex_input_includes_system_prompt_with_blank_line() {
        let out = build_codex_input("Be terse.", &[msg("user", "hi")], &SessionLimits::default());
        assert_eq!(out, "Be terse.\n\n[user]: hi\n");
    }

    #[test]
    fn codex_input_concatenates_messages_in_order() {
        let out = build_codex_input(
            "",
            &[msg("user", "q1"), msg("assistant", "a1"), msg("user", "q2")],
            &SessionLimits::default(),
        );
        assert_eq!(out, "[user]: q1\n[assistant]: a1\n[user]: q2\n");
    }

    // ---------------- build_claude_input ----------------

    #[test]
    fn claude_input_emits_one_ndjson_line_terminated_by_newline() {
        let out = build_claude_input(&[msg("user", "hello")], None);
        assert!(out.ends_with('\n'));
        assert_eq!(out.matches('\n').count(), 1);
    }

    #[test]
    fn claude_input_concatenates_history_as_single_user_turn() {
        let out = build_claude_input(
            &[msg("user", "q1"), msg("assistant", "a1"), msg("user", "q2")],
            None,
        );
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        let parts = v["message"]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(
            parts[0]["text"].as_str().unwrap(),
            "[user]: q1\n[assistant]: a1\n[user]: q2"
        );
    }

    #[test]
    fn claude_input_appends_image_part_when_screenshot_present() {
        let out = build_claude_input(&[msg("user", "look")], Some("AAAAFAKE=="));
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        let parts = v["message"]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["type"], "image");
        assert_eq!(parts[1]["source"]["type"], "base64");
        assert_eq!(parts[1]["source"]["media_type"], "image/png");
        assert_eq!(parts[1]["source"]["data"], "AAAAFAKE==");
    }

    #[test]
    fn claude_input_omits_image_when_no_screenshot() {
        let out = build_claude_input(&[msg("user", "hi")], None);
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        let parts = v["message"]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
    }

    // ---------------- parse_claude_line ----------------

    #[test]
    fn parse_claude_line_extracts_text_delta() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"PONG"}}}"#;
        assert_eq!(
            parse_claude_line(line),
            Some(Parsed::Text("PONG".to_owned()))
        );
    }

    #[test]
    fn parse_claude_line_ignores_non_text_deltas() {
        let start = r#"{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}}}"#;
        let block = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#;
        assert_eq!(parse_claude_line(start), None);
        assert_eq!(parse_claude_line(block), None);
    }

    #[test]
    fn parse_claude_line_ignores_system_and_assistant_frames() {
        let system = r#"{"type":"system","subtype":"init","session_id":"x"}"#;
        let assistant = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"PONG"}]}}"#;
        assert_eq!(parse_claude_line(system), None);
        assert_eq!(parse_claude_line(assistant), None);
    }

    #[test]
    fn parse_claude_line_treats_successful_result_as_stream_end() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"PONG"}"#;
        assert_eq!(parse_claude_line(line), None);
    }

    #[test]
    fn parse_claude_line_surfaces_error_result_message() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"error":"quota exceeded"}"#;
        assert_eq!(
            parse_claude_line(line),
            Some(Parsed::Error("quota exceeded".to_owned()))
        );
    }

    #[test]
    fn parse_claude_line_uses_fallback_when_error_result_has_no_message() {
        let line = r#"{"type":"result","is_error":true}"#;
        assert_eq!(
            parse_claude_line(line),
            Some(Parsed::Error("Unknown Claude CLI error".to_owned()))
        );
    }

    #[test]
    fn parse_claude_line_skips_malformed_or_empty_lines() {
        assert_eq!(parse_claude_line("not json"), None);
        assert_eq!(parse_claude_line(""), None);
    }

    const CODEX_EVENTS: &str = concat!(
        "{\"type\":\"thread.started\",\"thread_id\":\"test-thread\"}\n",
        "{\"type\":\"turn.started\"}\n",
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"aggregated_output\":\"private tool output\"}}\n",
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"PONG\"}}\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}\n",
    );

    #[test]
    fn codex_json_only_emits_assistant_text_and_records_completed_session() {
        let mut output = CodexOutput::default();
        let chunks: Vec<_> = CODEX_EVENTS
            .lines()
            .filter_map(|line| output.parse_line(line))
            .collect();
        assert_eq!(chunks, [Parsed::Text("PONG".to_owned())]);
        assert_eq!(output.finish(None).unwrap(), "test-thread");
        assert!(output.finish(Some("other-thread")).is_err());
    }

    #[test]
    fn codex_json_rejects_errors_malformed_and_incomplete_turns() {
        let mut output = CodexOutput::default();
        assert!(output.finish(None).is_err());
        for line in [
            "not JSON",
            r#"{"type":"turn.failed","error":{"message":"failed inference"}}"#,
        ] {
            assert!(matches!(output.parse_line(line), Some(Parsed::Error(_))));
        }
        output.parse_line(r#"{"type":"thread.started","thread_id":"x"}"#);
        output.parse_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"partial"}}"#,
        );
        assert!(output
            .finish(None)
            .unwrap_err()
            .contains("without completing"));
    }

    #[test]
    fn codex_error_notice_is_retained_if_the_turn_never_completes() {
        let mut output = CodexOutput::default();
        assert_eq!(
            output.parse_line(r#"{"type":"error","message":"quota exceeded"}"#),
            None
        );
        let error = output.finish(None).unwrap_err();
        assert!(error.contains("without completing"), "{error}");
        assert!(error.contains("quota exceeded"), "{error}");
    }

    #[tokio::test]
    async fn codex_reconnect_notice_can_recover_without_entering_the_answer() {
        let events = CODEX_EVENTS.replace(
            "{\"type\":\"turn.started\"}\n",
            concat!(
                "{\"type\":\"turn.started\"}\n",
                "{\"type\":\"error\",\"message\":\"Reconnecting... 1/1 (stream disconnected before completion: stream closed before response.completed)\"}\n",
            ),
        );
        for structured in [false, true] {
            let mut command = fake_cli(&events, "", 0);
            let mut output = CodexOutput {
                structured,
                ..Default::default()
            };
            let mut answer = String::new();
            run_cli(
                &mut command,
                "prompt".to_owned(),
                |chunk| {
                    answer.push_str(&chunk);
                    Ok(())
                },
                |line| output.parse_line(line),
                "fixture",
            )
            .await
            .unwrap();
            assert_eq!(output.finish(Some("test-thread")).unwrap(), "test-thread");
            assert_eq!(output.last_message, "PONG");
            assert_eq!(answer, if structured { "" } else { "PONG" });
        }
    }

    #[test]
    fn codex_transport_keeps_chatgpt_auth_and_bounds_retries() {
        for thread in [None, Some("session-id")] {
            let args = codex_args("work dir", thread, Some("image.png"), Some("schema.json"));
            let overrides = args
                .windows(2)
                .filter(|pair| pair[0] == "-c")
                .map(|pair| pair[1].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let config: toml::Value = toml::from_str(&overrides).unwrap();
            assert_eq!(config["forced_login_method"].as_str(), Some("chatgpt"));
            let provider_id = config["model_provider"].as_str().unwrap();
            assert_ne!(
                provider_id, "openai",
                "Built-in providers cannot be overridden"
            );
            let provider = &config["model_providers"][provider_id];
            assert_eq!(
                provider["base_url"].as_str(),
                Some("https://chatgpt.com/backend-api/codex")
            );
            assert_eq!(provider["requires_openai_auth"].as_bool(), Some(true));
            assert_eq!(provider["supports_websockets"].as_bool(), Some(false));
            assert_eq!(provider["wire_api"].as_str(), Some("responses"));
            assert_eq!(provider["request_max_retries"].as_integer(), Some(0));
            assert_eq!(provider["stream_max_retries"].as_integer(), Some(1));
            assert_eq!(
                config["features"]["unbounded_connection_retries"].as_bool(),
                Some(false)
            );
            assert!(provider.get("env_key").is_none());
            assert!(provider.get("experimental_bearer_token").is_none());
            assert!(config.get("model").is_none());
            assert!(config.get("model_reasoning_effort").is_none());
        }
    }

    #[test]
    fn codex_image_arguments_keep_resume_before_image_and_stdin_unambiguous() {
        let image = r"C:\Users\O'Brien\game shots\frame.png";
        let fresh = codex_args("work dir", None, Some(image), None);
        assert_eq!(&fresh[fresh.len() - 4..], ["--image", image, "--", "-"]);
        let resumed = codex_args("work dir", Some("session-id"), Some(image), None);
        let resume = resumed.iter().position(|arg| arg == "resume").unwrap();
        let image_option = resumed.iter().position(|arg| arg == "--image").unwrap();
        assert_eq!(resumed[resume + 1], "session-id");
        assert!(image_option > resume + 1);
        assert_eq!(&resumed[resumed.len() - 2..], ["--", "-"]);
        assert!(!resumed
            .iter()
            .any(|arg| arg == "--last" || arg == "--model"));
        assert!(resumed
            .iter()
            .any(|arg| arg == "forced_login_method=\"chatgpt\""));
        let command = codex_command(CliMode::Wsl, &resumed);
        let arguments: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&arguments[..3], ["--", "bash", "-ic"]);
        assert!(arguments[3].contains(&shell_escape(image)));
    }

    fn sample_session() -> (CliConfig, CodexSession, Vec<ChatMessage>) {
        let cfg = CliConfig {
            codex: CliMode::Native,
            codex_workdir: "test-work".to_owned(),
            ..Default::default()
        };
        let messages = vec![
            msg("user", "first"),
            msg("assistant", "answer"),
            msg("user", "next"),
        ];
        let session = CodexSession {
            thread_id: "test-thread".to_owned(),
            conversation_id: 7,
            mode: cfg.codex,
            workdir: cfg.codex_workdir.clone(),
            system_prompt: "game".to_owned(),
            notebook_identity: String::new(),
            expected_messages: 2,
            expected_history: history_fingerprint(&messages[..1], Some("answer")),
            turns: 1,
            image_turns: 1,
        };
        (cfg, session, messages)
    }

    #[test]
    fn notebook_identity_changes_require_a_fresh_codex_session() {
        let (cfg, mut session, messages) = sample_session();
        let limits = SessionLimits::default();
        session.notebook_identity = "project:run-and-brief".to_owned();
        assert!(session.can_resume(&cfg, "game", &messages, 7, &limits, "project:run-and-brief"));
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &limits, "project:new-run"));
        assert!(!session.can_resume(
            &cfg,
            "game",
            &messages,
            7,
            &limits,
            "other-project:run-and-brief"
        ));
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &limits, ""));
    }

    #[test]
    fn image_and_schema_paths_are_distinct_argv_after_resume() {
        let image = "/mnt/c/Users/O'Brien/game shots/frame.png";
        let schema = "/mnt/c/Users/O'Brien/AppData/Local/Temp/notebook.json";
        for thread in [None, Some("session-id")] {
            let args = codex_args("work dir", thread, Some(image), Some(schema));
            let schema_option = args
                .iter()
                .position(|arg| arg == "--output-schema")
                .unwrap();
            assert_eq!(args[schema_option + 1], schema);
            if thread.is_some() {
                let resume = args.iter().position(|arg| arg == "resume").unwrap();
                assert!(schema_option > resume + 1);
            }
            assert_eq!(&args[args.len() - 4..], ["--image", image, "--", "-"]);
            let native = codex_command(CliMode::Native, &args);
            assert_eq!(native.as_std().get_args().count(), args.len());
            let wsl = codex_command(CliMode::Wsl, &args);
            let invocation = wsl.as_std().get_args().last().unwrap().to_string_lossy();
            assert!(invocation.contains(&shell_escape(schema)));
            assert!(invocation.contains(&shell_escape(image)));
        }
        let file = CodexFile::create(notebook::RESPONSE_SCHEMA.as_bytes(), "json").unwrap();
        let path = file.0.clone();
        assert!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).is_ok()
        );
        drop(file);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn structured_codex_reply_never_streams_metadata_or_commentary() {
        let reply = serde_json::json!({
            "answer": "Compare these two passives.",
            "checkpoint": notebook::Checkpoint { objective: "PRIVATE NOTEBOOK METADATA".to_owned(), ..Default::default() }
        });
        let event = serde_json::json!({"type": "item.completed", "item": {"type": "agent_message", "text": reply.to_string()}});
        let prefix = CODEX_EVENTS
            .lines()
            .filter(|line| !line.contains("turn.completed"))
            .collect::<Vec<_>>()
            .join("\n");
        let events = format!("{prefix}\n{event}\n{{\"type\":\"turn.completed\"}}\n");
        let mut command = fake_cli(&events, "", 0);
        let mut output = CodexOutput {
            structured: true,
            ..Default::default()
        };
        let mut streamed = String::new();
        run_cli(
            &mut command,
            "input".to_owned(),
            |chunk| {
                streamed.push_str(&chunk);
                Ok(())
            },
            |line| output.parse_line(line),
            "fixture",
        )
        .await
        .unwrap();
        output.finish(Some("test-thread")).unwrap();
        assert!(streamed.is_empty());
        let decoded = notebook::decode_reply(&output.last_message).unwrap();
        assert_eq!(decoded.answer, "Compare these two passives.");
        let (cfg, mut session, mut messages) = sample_session();
        session.expected_history = history_fingerprint(&messages[..1], Some(&decoded.answer));
        messages[1].content = decoded.answer;
        assert!(session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
        messages[1].content = reply.to_string();
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
    }

    #[test]
    fn codex_resume_rejects_changed_history_conversation_game_and_cli() {
        let (mut cfg, session, mut messages) = sample_session();
        assert!(session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
        assert!(!session.can_resume(&cfg, "game", &messages, 8, &SessionLimits::default(), ""));
        assert!(!session.can_resume(
            &cfg,
            "other game",
            &messages,
            7,
            &SessionLimits::default(),
            ""
        ));
        assert!(!session.can_resume(
            &cfg,
            "game",
            &messages[..2],
            7,
            &SessionLimits::default(),
            ""
        ));
        messages[1].content = "edited answer".to_owned();
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
        messages[1].content = "answer".to_owned();
        cfg.codex = CliMode::Wsl;
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
        cfg.codex = CliMode::Native;
        cfg.codex_workdir = "other-work".to_owned();
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
    }

    #[test]
    fn codex_rolls_session_at_image_or_total_turn_limit() {
        let (cfg, mut session, messages) = sample_session();
        session.image_turns = SessionLimits::default().max_image_turns;
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
        session.image_turns = 0;
        session.turns = SessionLimits::default().max_turns;
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
    }

    #[test]
    fn codex_resume_rejects_provider_detour_or_non_user_suffix() {
        let (cfg, session, mut messages) = sample_session();
        messages.extend([
            msg("assistant", "other provider reply"),
            msg("user", "back to Codex"),
        ]);
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
        messages.truncate(3);
        messages[2].role = "assistant".to_owned();
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &SessionLimits::default(), ""));
    }

    #[test]
    fn codex_fresh_handoff_bounds_history_and_preserves_unicode() {
        let messages: Vec<_> = (0..20).map(|i| msg("user", &format!("turn-{i}"))).collect();
        let input = build_codex_input("system", &messages, &SessionLimits::default());
        assert!(!input.contains("[user]: turn-7\n"));
        assert!(input.contains("[user]: turn-8\n"));
        assert!(input.ends_with("[user]: turn-19\n"));
        let long = build_codex_input(
            "",
            &[msg("assistant", &"🦀".repeat(20_000)), msg("user", "next")],
            &SessionLimits::default(),
        );
        // Allow the short handoff annotation in addition to the text budget.
        assert!(long.chars().count() < SessionLimits::default().handoff_chars + 200);
        assert!(long.contains("🦀\n"));
        assert!(long.ends_with("[user]: next\n"));
    }

    #[test]
    fn edited_limits_change_rollover_and_text_handoff() {
        let (cfg, mut session, messages) = sample_session();
        session.turns = 16;
        session.image_turns = 8;
        let mut limits = SessionLimits {
            max_turns: 32,
            max_image_turns: 12,
            ..Default::default()
        };
        assert!(session.can_resume(&cfg, "game", &messages, 7, &limits, ""));
        limits.max_image_turns = 8;
        assert!(!session.can_resume(&cfg, "game", &messages, 7, &limits, ""));
        limits.handoff_messages = 1;
        let input = build_codex_input("updated instructions", &messages, &limits);
        assert!(input.starts_with("updated instructions\n\n"));
        assert!(!input.contains("[user]: first"));
        assert!(input.ends_with("[user]: next\n"));
        limits.handoff_chars = 256;
        let input = build_codex_input(
            "",
            &[msg("assistant", &"界".repeat(400)), msg("user", "next")],
            &limits,
        );
        assert!(input.chars().count() < 400);
        assert!(input.ends_with("[user]: next\n"));
        let question = format!("Preserve this correction: {}", "界".repeat(400));
        let input = build_codex_input("", &[msg("user", &question)], &limits);
        assert!(input.contains(&question));
    }

    #[test]
    fn codex_screenshot_temp_file_is_removed_on_drop() {
        let data = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture");
        let image = CodexFile::from_base64(&data).unwrap();
        let path = image.0.clone();
        assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG\r\n\x1a\nfixture");
        drop(image);
        assert!(!path.exists());
        assert!(CodexFile::from_base64("not base64").is_err());
        assert!(CodexFile::from_base64("aGVsbG8=").is_err());
    }

    #[tokio::test]
    async fn codex_input_error_invalidates_cached_session_before_spawn() {
        let (cfg, session, messages) = sample_session();
        let mut session = Some(session);
        let error = stream_codex(
            &cfg,
            "game",
            &messages,
            Some("invalid"),
            7,
            &mut session,
            &SessionLimits::default(),
            None,
            |_| Ok(()),
        )
        .await;
        assert!(error.is_err());
        assert!(session.is_none());
    }

    /// Subprocess fixtures exercise pipes and exit handling without contacting
    /// a provider or depending on a user's installed Codex binary.
    fn fake_cli(stdout: &str, stderr: &str, exit: i32) -> Command {
        #[cfg(windows)]
        {
            let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
            let script = format!(
                "$null = [Console]::In.ReadToEnd(); [Console]::Out.Write({}); [Console]::Error.Write({}); exit {exit}",
                quote(stdout), quote(stderr),
            );
            let mut cmd = Command::new("powershell.exe");
            cmd.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ]);
            cmd
        }
        #[cfg(not(windows))]
        {
            let script = format!(
                "cat >/dev/null; printf %s {}; printf %s {} >&2; exit {exit}",
                shell_escape(stdout),
                shell_escape(stderr)
            );
            let mut cmd = Command::new("sh");
            cmd.args(["-c", &script]);
            cmd
        }
    }

    #[tokio::test]
    async fn codex_subprocess_reads_json_and_requires_successful_exit() {
        let mut command = fake_cli(CODEX_EVENTS, "", 0);
        let mut output = CodexOutput::default();
        let mut answer = String::new();
        run_cli(
            &mut command,
            "prompt via stdin".to_owned(),
            |chunk| {
                answer.push_str(&chunk);
                Ok(())
            },
            |line| output.parse_line(line),
            "fixture",
        )
        .await
        .unwrap();
        assert_eq!(answer, "PONG");
        assert_eq!(output.finish(None).unwrap(), "test-thread");

        let mut command = fake_cli(CODEX_EVENTS, "synthetic failure", 7);
        let mut output = CodexOutput::default();
        let error = run_cli(
            &mut command,
            "prompt".to_owned(),
            |_| Ok(()),
            |line| output.parse_line(line),
            "fixture",
        )
        .await
        .unwrap_err();
        assert!(error.contains("synthetic failure"), "{error}");
    }

    #[tokio::test]
    async fn codex_subprocess_surfaces_protocol_and_callback_failures() {
        let mut command = fake_cli(
            concat!(
                "{\"type\":\"error\",\"message\":\"Reconnecting... 1/1\"}\n",
                "{\"type\":\"turn.failed\",\"error\":{\"message\":\"inference failed\"}}\n",
            ),
            "",
            0,
        );
        let mut output = CodexOutput::default();
        let error = run_cli(
            &mut command,
            "prompt".to_owned(),
            |_| Ok(()),
            |line| output.parse_line(line),
            "fixture",
        )
        .await
        .unwrap_err();
        assert_eq!(error, "inference failed");

        let mut command = fake_cli(CODEX_EVENTS, "", 0);
        let mut output = CodexOutput::default();
        let error = run_cli(
            &mut command,
            "prompt".to_owned(),
            |_| Err("overlay gone".to_owned()),
            |line| output.parse_line(line),
            "fixture",
        )
        .await
        .unwrap_err();
        assert_eq!(error, "overlay gone");
    }
}
