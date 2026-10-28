//! A small wrapper around the locally installed Claude Code CLI (`claude`), used in print mode
//! for one-shot structured requests — the app's "model access" without an API key. Modelled on
//! the Agent SDK's subprocess transport and on `silverstein/minutes`' lean invocation (see
//! `docs/research-claude-cli-and-interpretation.md`):
//!
//! ```text
//! claude -p --output-format json --model <m> --safe-mode --setting-sources "" \
//!        --strict-mcp-config --tools "" --disable-slash-commands --no-session-persistence \
//!        --max-turns N --max-budget-usd X --system-prompt <text> --json-schema <schema>
//! ```
//!
//! The user prompt (which carries the transcript) goes over **stdin** (no argv limits, no
//! quoting); stdout is one JSON `result` object. The child gets an **allowlisted
//! environment** ([`child_env`]): `HOME`, `USER`, `LOGNAME`, `TMPDIR`, `LANG`, `LC_ALL` /
//! `LC_CTYPE`, `TZ`, `CLAUDE_CONFIG_DIR` (when set), a fixed `PATH`, and the CLI's own
//! telemetry / error-reporting / auto-update switches turned off — nothing else of the app's
//! environment (so no `ANTHROPIC_BASE_URL` or proxy redirects the transcript, no parent
//! Claude Code session variables leak in; proxies only with `OATS_CLAUDE_PASS_PROXY=1`). The
//! working directory is an empty one (`<data dir>/claude-work`) so no project CLAUDE.md or
//! `.mcp.json` is loaded and nothing of the app's data is within reach. A timeout kills the
//! child (and its process group).

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Environment variable naming the `claude` binary (default: `~/.local/bin/claude` when it
/// exists, else `claude` on PATH).
pub const BIN_ENV: &str = "OATS_CLAUDE_BIN";
/// Environment variable overriding the model alias for every tier (`haiku`, `sonnet`, …).
pub const MODEL_ENV: &str = "OATS_CLAUDE_MODEL";
/// `OATS_CLAUDE_PASS_PROXY=1` passes [`PROXY_VARS`] through to the child (off by default).
pub const PASS_PROXY_ENV: &str = "OATS_CLAUDE_PASS_PROXY";

/// The app's environment variables the child may see (when set).
pub const PASS_VARS: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "CLAUDE_CONFIG_DIR",
];
/// Passed only with `OATS_CLAUDE_PASS_PROXY=1`: anything that changes where requests go.
pub const PROXY_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "ANTHROPIC_BASE_URL",
    "NODE_EXTRA_CA_CERTS",
];
/// The child's `PATH`, before the claude binary's own directory.
pub const CHILD_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin";
/// Set on every child: no telemetry, no error reports (which could carry prompt fragments),
/// no non-essential traffic, no self-update from inside the app.
pub const CHILD_FIXED_ENV: &[(&str, &str)] = &[
    ("CLAUDE_CODE_ENTRYPOINT", "oats-notes"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
];

/// The complete environment of a `claude` child, from `get` (the app's environment) and the
/// resolved binary: the allowlist, the proxy variables only when `OATS_CLAUDE_PASS_PROXY=1`,
/// a fixed `PATH` plus the binary's directory, and [`CHILD_FIXED_ENV`]. Pure, so it is
/// testable without touching the process environment.
pub fn child_env(get: &dyn Fn(&str) -> Option<OsString>, binary: &Path) -> Vec<(String, OsString)> {
    let mut env: Vec<(String, OsString)> = Vec::new();
    for k in PASS_VARS {
        if let Some(v) = get(k) {
            env.push(((*k).to_owned(), v));
        }
    }
    if get(PASS_PROXY_ENV).is_some_and(|v| v == "1") {
        for k in PROXY_VARS {
            if let Some(v) = get(k) {
                env.push(((*k).to_owned(), v));
            }
        }
    }
    let mut path = OsString::from(CHILD_PATH);
    if let Some(dir) = binary.parent().filter(|d| d.is_absolute()) {
        path.push(":");
        path.push(dir.as_os_str());
    }
    env.push(("PATH".to_owned(), path));
    for (k, v) in CHILD_FIXED_ENV {
        env.push(((*k).to_owned(), OsString::from(v)));
    }
    env
}

/// Resolve the configured binary to an absolute, canonical path: an absolute path is
/// canonicalised (symlinks followed); a bare name (`claude`) is looked up on `PATH`; any other
/// relative path is refused (it would depend on the working directory).
pub fn resolve_binary(configured: &Path) -> Result<PathBuf, CliError> {
    let not_found = |why: &str| {
        CliError::NotFound(
            configured.to_path_buf(),
            std::io::Error::new(std::io::ErrorKind::NotFound, why.to_owned()),
        )
    };
    let candidate = if configured.is_absolute() {
        configured.to_path_buf()
    } else if configured.components().count() == 1 {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .filter(|d| d.is_absolute())
            .map(|d| d.join(configured))
            .find(|p| p.is_file())
            .ok_or_else(|| not_found("not found on PATH"))?
    } else {
        return Err(not_found(
            "a relative path is refused; give an absolute one",
        ));
    };
    let canonical = std::fs::canonicalize(&candidate)
        .map_err(|e| CliError::NotFound(configured.to_path_buf(), e))?;
    if !canonical.is_file() {
        return Err(not_found("not a file"));
    }
    Ok(canonical)
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("`{0}` could not be started: {1} (set {BIN_ENV} to the claude binary)")]
    NotFound(PathBuf, std::io::Error),
    #[error("claude is not logged in: {0}")]
    NotLoggedIn(String),
    #[error("claude is rate limited: {0}")]
    RateLimited(String),
    /// The CLI could not reach the service (no network, DNS, connection refused/reset).
    #[error("claude cannot reach the service (offline?): {0}")]
    Offline(String),
    #[error("claude did not finish within {0:?}")]
    Timeout(Duration),
    /// The caller cancelled the request (the app is quitting); the child was killed.
    #[error("the claude request was cancelled")]
    Cancelled,
    #[error("claude exited with {code}: {stderr}")]
    Exit { code: i32, stderr: String },
    #[error("claude produced no usable JSON: {0}")]
    BadOutput(String),
    #[error("claude refused or failed the request: {0}")]
    Refused(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl CliError {
    /// Why claude as a whole can't be used right now — not this request's fault: missing,
    /// logged out, offline or rate limited. `None` for errors specific to one request.
    pub fn unavailable_reason(&self) -> Option<&'static str> {
        match self {
            CliError::NotFound(..) => Some("not installed"),
            CliError::NotLoggedIn(_) => Some("not logged in"),
            CliError::Offline(_) => Some("offline"),
            CliError::RateLimited(_) => Some("rate limited"),
            _ => None,
        }
    }

    /// The variant's name, for logs that must not carry CLI output (which can echo model
    /// output derived from the transcript).
    pub fn kind(&self) -> &'static str {
        match self {
            CliError::NotFound(..) => "not found",
            CliError::NotLoggedIn(_) => "not logged in",
            CliError::RateLimited(_) => "rate limited",
            CliError::Offline(_) => "offline",
            CliError::Timeout(_) => "timeout",
            CliError::Cancelled => "cancelled",
            CliError::Exit { .. } => "non-zero exit",
            CliError::BadOutput(_) => "bad output",
            CliError::Refused(_) => "refused",
            CliError::Io(_) => "io",
        }
    }
}

/// A one-shot request.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub system_prompt: String,
    /// Sent over stdin.
    pub user_prompt: String,
    /// `--json-schema`; the validated object comes back as [`Response::structured`].
    pub json_schema: Option<serde_json::Value>,
    /// Overrides [`ClaudeCli::model`] for this request (a tier's model).
    pub model: Option<String>,
    /// `--max-turns` (default 2: the answer plus the structured-output tool call).
    pub max_turns: Option<u32>,
}

/// The parsed `result` object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Response {
    pub text: String,
    pub structured: Option<serde_json::Value>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i64>,
    pub num_turns: Option<i64>,
    pub session_id: Option<String>,
    /// The model that actually answered (first key of `model_usage`), if reported.
    pub model: Option<String>,
    pub subtype: Option<String>,
}

/// `claude --version`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub raw: String,
}

impl Version {
    /// "2.1.312 (Claude Code)" → 2.1.312.
    pub fn parse(raw: &str) -> Option<Version> {
        let first = raw.split_whitespace().next()?;
        let mut parts = first.split('.').map(|p| p.parse::<u32>().ok());
        let major = parts.next()??;
        let minor = parts.next().flatten().unwrap_or(0);
        let patch = parts.next().flatten().unwrap_or(0);
        Some(Version {
            major,
            minor,
            patch,
            raw: raw.trim().to_owned(),
        })
    }
}

/// The CLI configuration.
#[derive(Clone, Debug)]
pub struct ClaudeCli {
    pub binary: PathBuf,
    /// Default model alias; `None` lets the CLI pick its default.
    pub model: Option<String>,
    pub timeout: Duration,
    /// `--max-budget-usd` per request.
    pub max_budget_usd: Option<f64>,
    /// Working directory for the child (created if missing); `None` = inherit.
    pub work_dir: Option<PathBuf>,
    /// Pass the isolation flags (`--safe-mode`, no tools, no MCP, no settings, no session
    /// file). Off only in tests with a fake binary that would choke on them — the fake here
    /// ignores its arguments, so it stays on.
    pub isolate: bool,
}

impl Default for ClaudeCli {
    fn default() -> Self {
        ClaudeCli {
            binary: find_binary(),
            model: std::env::var(MODEL_ENV).ok().filter(|m| !m.is_empty()),
            timeout: Duration::from_secs(180),
            max_budget_usd: Some(0.50),
            work_dir: None,
            isolate: true,
        }
    }
}

/// `OATS_CLAUDE_BIN`, else `~/.local/bin/claude` if it exists (a GUI app launched from the Dock
/// doesn't have it on PATH), else `claude` on PATH.
pub fn find_binary() -> PathBuf {
    if let Some(p) = std::env::var_os(BIN_ENV).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let local = Path::new(&home).join(".local").join("bin").join("claude");
        if local.is_file() {
            return local;
        }
    }
    PathBuf::from("claude")
}

/// Classify a failure message from the CLI (its `result` text or stderr).
fn classify(msg: &str, status: Option<i64>) -> Option<CliError> {
    let lower = msg.to_lowercase();
    let auth = matches!(status, Some(401) | Some(403))
        || lower.contains("not logged in")
        || lower.contains("/login")
        || lower.contains("invalid api key")
        || lower.contains("authentication")
        || lower.contains("oauth token")
        || lower.contains("unauthorized");
    if auth {
        return Some(CliError::NotLoggedIn(msg.trim().to_owned()));
    }
    let limited = matches!(status, Some(429) | Some(529))
        || lower.contains("rate limit")
        || lower.contains("usage limit")
        || lower.contains("overloaded")
        || lower.contains("too many requests");
    if limited {
        return Some(CliError::RateLimited(msg.trim().to_owned()));
    }
    let offline = [
        "econnrefused",
        "enotfound",
        "etimedout",
        "econnreset",
        "eai_again",
        "enetunreach",
        "getaddrinfo",
        "fetch failed",
        "unable to connect",
        "connection error",
        "network error",
        "socket hang up",
        "you appear to be offline",
    ]
    .iter()
    .any(|p| lower.contains(p));
    if offline {
        return Some(CliError::Offline(msg.trim().to_owned()));
    }
    None
}

/// Find the `result` object in what the CLI printed: a single object (`--output-format json`),
/// an array of messages, or NDJSON lines.
fn find_result(stdout: &str) -> Option<serde_json::Value> {
    let is_result =
        |v: &serde_json::Value| v.get("type").and_then(|t| t.as_str()) == Some("result");
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        match v {
            serde_json::Value::Array(items) => {
                return items
                    .iter()
                    .rev()
                    .find(|i| is_result(i))
                    .cloned()
                    .or_else(|| items.last().cloned());
            }
            other => return Some(other),
        }
    }
    let mut last = None;
    for line in stdout.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if is_result(&v) {
                return Some(v);
            }
            last = Some(v);
        }
    }
    last
}

fn parse_response(v: &serde_json::Value) -> Response {
    let model = v
        .get("model_usage")
        .or_else(|| v.get("modelUsage"))
        .and_then(|m| m.as_object())
        .and_then(|m| m.keys().next().cloned());
    Response {
        text: v
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .to_owned(),
        structured: v.get("structured_output").cloned(),
        cost_usd: v.get("total_cost_usd").and_then(|c| c.as_f64()),
        duration_ms: v.get("duration_ms").and_then(|d| d.as_i64()),
        num_turns: v.get("num_turns").and_then(|n| n.as_i64()),
        session_id: v
            .get("session_id")
            .and_then(|s| s.as_str())
            .map(str::to_owned),
        model,
        subtype: v.get("subtype").and_then(|s| s.as_str()).map(str::to_owned),
    }
}

/// Read a pipe to the end on a thread (so a full pipe can never block the child).
fn drain<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

impl ClaudeCli {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.env_clear()
            .envs(child_env(&|k| std::env::var_os(k), &self.binary))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &self.work_dir {
            if let Err(e) = oats_core::paths::secure_dir(dir) {
                log::warn!("claude work dir {}: {e}", dir.display());
            }
            cmd.current_dir(dir);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd
    }

    fn spawn_err(&self, e: std::io::Error) -> CliError {
        if e.kind() == std::io::ErrorKind::NotFound {
            CliError::NotFound(self.binary.clone(), e)
        } else {
            CliError::Io(e)
        }
    }

    /// `claude --version`: proves the binary runs. (Login state only shows on a real request.)
    pub fn available(&self) -> Result<Version, CliError> {
        let mut cmd = self.command();
        cmd.arg("--version").stdin(Stdio::null());
        let child = cmd.spawn().map_err(|e| self.spawn_err(e))?;
        let out = wait_with_timeout(child, Duration::from_secs(20), None)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if out.code != 0 {
            return Err(CliError::Exit {
                code: out.code,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            });
        }
        Version::parse(&stdout).ok_or_else(|| CliError::BadOutput(stdout.trim().to_owned()))
    }

    /// Run one request and parse the result.
    pub fn run(&self, req: &Request) -> Result<Response, CliError> {
        self.run_cancellable(req, &AtomicBool::new(false))
    }

    /// [`Self::run`], but setting `cancel` (from another thread) kills the child and returns
    /// [`CliError::Cancelled`] within a poll interval — so quitting the app never waits out a
    /// long request (and never leaves an orphaned `claude` running).
    pub fn run_cancellable(
        &self,
        req: &Request,
        cancel: &AtomicBool,
    ) -> Result<Response, CliError> {
        let mut cmd = self.command();
        cmd.arg("-p").args(["--output-format", "json"]);
        if let Some(model) = req.model.as_ref().or(self.model.as_ref()) {
            cmd.args(["--model", model]);
        }
        if self.isolate {
            cmd.args([
                "--safe-mode",
                "--setting-sources",
                "",
                "--strict-mcp-config",
                "--tools",
                "",
                "--disable-slash-commands",
                "--no-session-persistence",
            ]);
        }
        cmd.args(["--max-turns", &req.max_turns.unwrap_or(2).to_string()]);
        if let Some(b) = self.max_budget_usd {
            cmd.args(["--max-budget-usd", &format!("{b:.2}")]);
        }
        cmd.args(["--system-prompt", &req.system_prompt]);
        if let Some(schema) = &req.json_schema {
            cmd.args(["--json-schema", &schema.to_string()]);
        }
        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|e| self.spawn_err(e))?;
        if let Some(mut stdin) = child.stdin.take() {
            let prompt = req.user_prompt.clone();
            std::thread::spawn(move || {
                let _ = stdin.write_all(prompt.as_bytes());
                drop(stdin);
            });
        }
        let out = wait_with_timeout(child, self.timeout, Some(cancel))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        log::debug!(
            "claude exited {} after {:.1}s ({} bytes out)",
            out.code,
            started.elapsed().as_secs_f32(),
            stdout.len()
        );
        if out.code != 0 {
            if let Some(e) = classify(&stderr, None).or_else(|| classify(&stdout, None)) {
                return Err(e);
            }
            return Err(CliError::Exit {
                code: out.code,
                stderr: if stderr.is_empty() {
                    stdout.chars().take(400).collect()
                } else {
                    stderr
                },
            });
        }
        let Some(result) = find_result(&stdout) else {
            let snippet: String = stdout.chars().take(300).collect();
            return Err(CliError::BadOutput(if snippet.is_empty() {
                stderr
            } else {
                snippet
            }));
        };
        let resp = parse_response(&result);
        let is_error = result
            .get("is_error")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if is_error {
            let status = result.get("api_error_status").and_then(|s| s.as_i64());
            let msg = if resp.text.is_empty() {
                format!("{:?}", resp.subtype)
            } else {
                resp.text.clone()
            };
            return Err(classify(&msg, status).unwrap_or(CliError::Refused(msg)));
        }
        if req.json_schema.is_some() && resp.structured.is_none() {
            let msg = if resp.text.is_empty() {
                format!(
                    "no structured output ({})",
                    resp.subtype.as_deref().unwrap_or("?")
                )
            } else {
                resp.text.clone()
            };
            // not an error result: the text is the model's own words (which may well mention
            // "authentication" or "rate limits"), so it is never classified as a CLI state
            return Err(CliError::Refused(msg));
        }
        Ok(resp)
    }
}

struct Output {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Collect a child's output, killing it (and its process group) past `timeout` or once
/// `cancel` is set.
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<Output, CliError> {
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child);
            return Err(CliError::Timeout(timeout));
        }
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            kill_tree(&mut child);
            return Err(CliError::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    Ok(Output {
        code: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // the child is its own process group (see `command`): signal the whole group
        let _ = Command::new("/bin/kill")
            .args(["-TERM", "--", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `claude`: a shell script that records its stdin and arguments, then runs `body`.
    fn fake(tag: &str, body: &str) -> (ClaudeCli, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "oats-brain-cli-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("claude");
        let log = dir.join("log");
        let _ = std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{log}.args'\ncat > '{log}.stdin'\n{body}\n",
                log = log.display()
            ),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        let cli = ClaudeCli {
            binary: script,
            model: Some("haiku".into()),
            timeout: Duration::from_secs(10),
            max_budget_usd: Some(0.25),
            work_dir: Some(dir.join("work")),
            isolate: true,
        };
        (cli, log)
    }

    fn request() -> Request {
        Request {
            system_prompt: "sys".into(),
            user_prompt: "hello transcript".into(),
            json_schema: Some(serde_json::json!({"type": "object"})),
            model: None,
            max_turns: None,
        }
    }

    #[test]
    fn happy_path_parses_result_and_passes_prompt_on_stdin() {
        let (cli, log) = fake(
            "ok",
            r#"echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"num_turns":1,"result":"done","session_id":"abc","total_cost_usd":0.0042,"structured_output":{"title":"T"},"model_usage":{"claude-haiku-4-5":{"inputTokens":1}}}'"#,
        );
        let r = cli.run(&request()).unwrap();
        assert_eq!(r.text, "done");
        assert_eq!(r.structured, Some(serde_json::json!({"title": "T"})));
        assert_eq!(r.cost_usd, Some(0.0042));
        assert_eq!(r.duration_ms, Some(1234));
        assert_eq!(r.num_turns, Some(1));
        assert_eq!(r.session_id.as_deref(), Some("abc"));
        assert_eq!(r.model.as_deref(), Some("claude-haiku-4-5"));
        let stdin = std::fs::read_to_string(format!("{}.stdin", log.display())).unwrap();
        assert_eq!(stdin, "hello transcript");
        let args = std::fs::read_to_string(format!("{}.args", log.display())).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args[0], "-p");
        assert!(args.windows(2).any(|w| w == ["--output-format", "json"]));
        assert!(args.windows(2).any(|w| w == ["--model", "haiku"]));
        assert!(args.windows(2).any(|w| w == ["--tools", ""]));
        assert!(args.windows(2).any(|w| w == ["--max-turns", "2"]));
        assert!(args.windows(2).any(|w| w == ["--max-budget-usd", "0.25"]));
        assert!(args.windows(2).any(|w| w == ["--system-prompt", "sys"]));
        assert!(args.contains(&"--json-schema"));
        assert!(args.contains(&"--safe-mode"));
        assert!(args.contains(&"--no-session-persistence"));
    }

    #[test]
    fn ndjson_and_array_outputs_are_accepted() {
        let (cli, _) = fake(
            "ndjson",
            r#"echo '{"type":"system","subtype":"init"}'; echo '{"type":"result","is_error":false,"result":"x","structured_output":{}}'"#,
        );
        assert_eq!(cli.run(&request()).unwrap().text, "x");
        let (cli, _) = fake(
            "array",
            r#"echo '[{"type":"assistant"},{"type":"result","is_error":false,"result":"y","structured_output":{}}]'"#,
        );
        assert_eq!(cli.run(&request()).unwrap().text, "y");
    }

    #[test]
    fn non_zero_exit_is_reported_with_stderr_and_auth_is_classified() {
        let (cli, _) = fake("exit", "echo 'boom' >&2; exit 3");
        match cli.run(&request()) {
            Err(CliError::Exit { code: 3, stderr }) => assert_eq!(stderr, "boom"),
            other => unreachable!("{other:?}"),
        }
        let (cli, _) = fake(
            "auth",
            "echo 'Not logged in · Please run /login' >&2; exit 1",
        );
        assert!(matches!(cli.run(&request()), Err(CliError::NotLoggedIn(_))));
        // the documented in-run failure: exit 0, is_error in the result
        let (cli, _) = fake(
            "limit",
            r#"echo '{"type":"result","subtype":"success","is_error":true,"result":"You have hit your usage limit","api_error_status":429}'"#,
        );
        assert!(matches!(cli.run(&request()), Err(CliError::RateLimited(_))));
        let (cli, _) = fake(
            "refused",
            r#"echo '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"something else"}'"#,
        );
        assert!(matches!(cli.run(&request()), Err(CliError::Refused(_))));
    }

    #[test]
    fn malformed_json_and_missing_structured_output_fail() {
        let (cli, _) = fake("garbage", "echo 'not json at all'");
        assert!(matches!(cli.run(&request()), Err(CliError::BadOutput(_))));
        let (cli, _) = fake(
            "nostruct",
            r#"echo '{"type":"result","is_error":false,"result":"I cannot do that"}'"#,
        );
        match cli.run(&request()) {
            Err(CliError::Refused(m)) => assert_eq!(m, "I cannot do that"),
            other => unreachable!("{other:?}"),
        }
        // without a schema, plain text is fine
        let (cli, _) = fake(
            "text",
            r#"echo '{"type":"result","is_error":false,"result":"plain"}'"#,
        );
        let r = cli
            .run(&Request {
                json_schema: None,
                ..request()
            })
            .unwrap();
        assert_eq!(r.text, "plain");
        assert_eq!(r.structured, None);
    }

    #[test]
    fn timeout_kills_the_child() {
        let (mut cli, _) = fake("slow", "sleep 5; echo '{}'");
        cli.timeout = Duration::from_millis(300);
        let t = Instant::now();
        assert!(matches!(cli.run(&request()), Err(CliError::Timeout(_))));
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn cancel_kills_the_child_promptly() {
        let (cli, _) = fake("cancel", "sleep 5; echo '{}'");
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
        });
        let t = Instant::now();
        assert!(matches!(
            cli.run_cancellable(&request(), &cancel),
            Err(CliError::Cancelled)
        ));
        assert!(t.elapsed() < Duration::from_secs(3));
        let _ = setter.join();
    }

    #[test]
    fn model_text_without_structured_output_is_never_an_auth_error() {
        let (cli, _) = fake(
            "authtext",
            r#"echo '{"type":"result","is_error":false,"subtype":"error_max_turns","result":"We discussed the OAuth token authentication rework"}'"#,
        );
        assert!(matches!(cli.run(&request()), Err(CliError::Refused(_))));
    }

    #[test]
    fn version_and_missing_binary() {
        let (cli, _) = fake("version", "echo '2.1.312 (Claude Code)'");
        let v = cli.available().unwrap();
        assert_eq!((v.major, v.minor, v.patch), (2, 1, 312));
        assert_eq!(Version::parse("garbage"), None);
        let missing = ClaudeCli {
            binary: PathBuf::from("/nonexistent/claude-binary"),
            ..ClaudeCli::default()
        };
        assert!(matches!(missing.available(), Err(CliError::NotFound(..))));
        assert!(matches!(
            missing.run(&request()),
            Err(CliError::NotFound(..))
        ));
    }

    #[test]
    fn child_env_is_an_allowlist() {
        let parent: std::collections::HashMap<&str, &str> = [
            ("HOME", "/Users/me"),
            ("USER", "me"),
            ("LANG", "en_GB.UTF-8"),
            ("PATH", "/evil/bin:/usr/bin"),
            ("ANTHROPIC_BASE_URL", "https://elsewhere.example"),
            ("ANTHROPIC_API_KEY", "sk-not-mine"),
            ("HTTPS_PROXY", "http://proxy:8080"),
            ("CLAUDE_CODE_SESSION_ID", "parent-session"),
            ("CLAUDECODE", "1"),
            ("NODE_OPTIONS", "--require /tmp/x.js"),
            ("CLAUDE_CONFIG_DIR", "/Users/me/.claude-alt"),
        ]
        .into_iter()
        .collect();
        let bin = Path::new("/Users/me/.local/share/claude/versions/2.1.312");
        let env_of = |extra: Option<(&str, &str)>| {
            let get = |k: &str| -> Option<OsString> {
                match extra {
                    Some((ek, ev)) if ek == k => Some(OsString::from(ev)),
                    _ => parent.get(k).map(OsString::from),
                }
            };
            child_env(&get, bin)
        };
        let env = env_of(None);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        for k in &keys {
            assert!(
                PASS_VARS.contains(k)
                    || *k == "PATH"
                    || CHILD_FIXED_ENV.iter().any(|(f, _)| f == k),
                "{k} must not reach the child"
            );
        }
        for absent in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "HTTPS_PROXY",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDECODE",
            "NODE_OPTIONS",
        ] {
            assert!(!keys.contains(&absent), "{absent}");
        }
        let get = |k: &str| env.iter().find(|(e, _)| e == k).map(|(_, v)| v.clone());
        assert_eq!(get("HOME"), Some(OsString::from("/Users/me")));
        assert_eq!(
            get("CLAUDE_CONFIG_DIR"),
            Some(OsString::from("/Users/me/.claude-alt"))
        );
        assert_eq!(
            get("PATH"),
            Some(OsString::from(format!(
                "{CHILD_PATH}:/Users/me/.local/share/claude/versions"
            )))
        );
        assert_eq!(get("DISABLE_TELEMETRY"), Some(OsString::from("1")));
        assert_eq!(get("DISABLE_AUTOUPDATER"), Some(OsString::from("1")));
        // opt-in proxy pass-through
        let env = env_of(Some((PASS_PROXY_ENV, "1")));
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"HTTPS_PROXY") && keys.contains(&"ANTHROPIC_BASE_URL"));
        assert!(!keys.contains(&"ANTHROPIC_API_KEY") && !keys.contains(&"NODE_OPTIONS"));
    }

    /// The real child sees only the allowlist. The variables that must NOT pass are set on a
    /// re-run of this very test (setting them in-process would need `unsafe`).
    #[test]
    fn the_child_process_gets_only_the_allowlisted_environment() {
        const INNER: &str = "OATS_TEST_ENV_INNER";
        if std::env::var_os(INNER).is_none() {
            let exe = std::env::current_exe().unwrap();
            let out = Command::new(exe)
                .args([
                    "claude_cli::tests::the_child_process_gets_only_the_allowlisted_environment",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(INNER, "1")
                .env("ANTHROPIC_BASE_URL", "https://elsewhere.example")
                .env("HTTPS_PROXY", "http://proxy.example:8080")
                .env("CLAUDE_CODE_SESSION_ID", "parent-session")
                .env("CLAUDECODE", "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(stdout.contains("1 passed"), "{stdout}");
            return;
        }
        // the script dumps its environment next to itself ("$0.env")
        let (cli, _) = fake(
            "env",
            r#"/usr/bin/env > "$0.env"; echo '{"type":"result","is_error":false,"result":"ok","structured_output":{}}'"#,
        );
        cli.run(&request()).unwrap();
        let env = std::fs::read_to_string(cli.binary.with_extension("env")).unwrap();
        let keys: Vec<&str> = env.lines().filter_map(|l| l.split('=').next()).collect();
        for k in &keys {
            assert!(
                PASS_VARS.contains(k)
                    || *k == "PATH"
                    || *k == "PWD"
                    || *k == "SHLVL"
                    || *k == "_"
                    || CHILD_FIXED_ENV.iter().any(|(f, _)| f == k),
                "{k} reached the child: {env}"
            );
        }
        assert!(!env.contains("elsewhere.example") && !env.contains("proxy.example"));
        assert!(!keys.contains(&"CLAUDE_CODE_SESSION_ID") && !keys.contains(&"CLAUDECODE"));
        assert!(env.contains("DISABLE_ERROR_REPORTING=1"));
        assert!(env.contains("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1"));
        assert!(env.contains("CLAUDE_CODE_ENTRYPOINT=oats-notes"));
    }

    #[test]
    fn binaries_resolve_to_absolute_canonical_paths() {
        let (cli, _) = fake("resolve", "true");
        let resolved = resolve_binary(&cli.binary).unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, std::fs::canonicalize(&cli.binary).unwrap());
        assert!(matches!(
            resolve_binary(Path::new("./claude")),
            Err(CliError::NotFound(..))
        ));
        assert!(matches!(
            resolve_binary(Path::new("bin/claude")),
            Err(CliError::NotFound(..))
        ));
        // a bare name is looked up on PATH (`sh` is everywhere)
        let sh = resolve_binary(Path::new("sh")).unwrap();
        assert!(sh.is_absolute(), "{}", sh.display());
        assert!(matches!(
            resolve_binary(Path::new("/nonexistent/claude")),
            Err(CliError::NotFound(..))
        ));
    }

    #[test]
    fn network_failures_are_classified_offline() {
        let (cli, _) = fake(
            "offline",
            "echo 'API Error: Connection error. (getaddrinfo ENOTFOUND api.anthropic.com)' >&2; exit 1",
        );
        let e = cli.run(&request()).unwrap_err();
        assert!(matches!(e, CliError::Offline(_)), "{e:?}");
        assert_eq!(e.unavailable_reason(), Some("offline"));
        assert_eq!(CliError::Refused("x".into()).unavailable_reason(), None);
        assert_eq!(
            CliError::Exit {
                code: 1,
                stderr: "secret transcript".into()
            }
            .kind(),
            "non-zero exit"
        );
    }
}
