//! Wayland-native backend implementing the OpenAI Codex Linux Computer Use
//! (`@oai/sky`) command-line contract.
//!
//! The upstream `sky_linux_<arch>` binary drives the desktop through X11
//! (XTEST + XGetImage) and therefore cannot see or control native Wayland
//! windows. This binary is a drop-in replacement that speaks the identical CLI
//! and JSON protocol but performs capture and input through the
//! xdg-desktop-portal RemoteDesktop + ScreenCast interfaces, which is the
//! supported way to remote-control a Wayland session.
//!
//! Protocol (unchanged from upstream sky):
//!   argv:  [--client <c>] [--timeout-ms <n>] [--mouse-size-px <n>] <subcommand>
//!   stdin: JSON action input (see the FullDesktop.* types)
//!   stdout(get_screenshot): JSON `[{"filepath": "..."}]`
//!   errors: message on stderr, non-zero exit code
//!
//! To avoid a permission prompt on every single action, the first invocation
//! starts a long-lived daemon that owns one RemoteDesktop+ScreenCast session
//! (a single one-time portal grant, exactly like macOS Screen Recording +
//! Accessibility). Subsequent invocations forward their action to the daemon
//! over a per-user Unix socket.

mod keymap;
#[allow(dead_code)]
mod kwin;

use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 120_000;
const FIRST_START_TIMEOUT_MS: u64 = 180_000;
const DEADLINE_RESERVE_MS: u64 = 100;
const RESPONSE_RESERVE_MS: u64 = 20;
const FORCED_CLOSE_ATTEMPT_MS: u64 = 10;

#[derive(Clone, Copy, Debug)]
struct Deadline {
    end: std::time::Instant,
}

impl Deadline {
    fn for_request(timeout_ms: u64) -> (Self, Self) {
        Self::for_request_at(std::time::Instant::now(), timeout_ms)
    }

    fn for_request_at(now: std::time::Instant, timeout_ms: u64) -> (Self, Self) {
        let reserve_ms = DEADLINE_RESERVE_MS.min(timeout_ms / 2);
        let response_reserve_ms = RESPONSE_RESERVE_MS.min(reserve_ms / 2);
        let action_ms = timeout_ms - reserve_ms;
        (
            Self {
                end: now + Duration::from_millis(action_ms),
            },
            Self {
                end: now + Duration::from_millis(timeout_ms - response_reserve_ms),
            },
        )
    }

    fn remaining_at(&self, now: std::time::Instant) -> Result<Duration> {
        self.end
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
            .context("request deadline expired")
    }

    fn remaining(&self) -> Result<Duration> {
        self.remaining_at(std::time::Instant::now())
    }

    fn check(&self) -> Result<()> {
        self.remaining().map(|_| ())
    }
}

fn runtime_path(name: &str) -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .context("XDG_RUNTIME_DIR is required for the private computer-use runtime")?;
    let base = PathBuf::from(base);
    if !base.is_absolute() {
        bail!("XDG_RUNTIME_DIR must be an absolute path");
    }
    Ok(base.join(name))
}

fn socket_path() -> Result<PathBuf> {
    runtime_path("codex-sky-wayland.sock")
}

fn status_path() -> Result<PathBuf> {
    runtime_path("codex-sky-wayland.status")
}

/// Parsed command line following the upstream sky contract.
struct Cli {
    command: String,
    timeout_ms: u64,
}

fn parse_cli() -> Result<Cli> {
    parse_cli_from(std::env::args().skip(1))
}

fn parse_cli_from(args: impl IntoIterator<Item = String>) -> Result<Cli> {
    let mut args = args.into_iter().peekable();
    let mut command: Option<String> = None;
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--timeout-ms" => {
                let raw = args.next().context("--timeout-ms requires a value")?;
                timeout_ms = raw
                    .parse::<u64>()
                    .with_context(|| format!("invalid --timeout-ms value {raw:?}"))?;
                if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
                    bail!("--timeout-ms must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS}");
                }
            }
            "--client" | "--mouse-size-px" => {
                args.next()
                    .with_context(|| format!("{a} requires a value"))?;
            }
            "-h" | "--help" => {
                println!(
                    "sky_wayland <--client C> <--timeout-ms N> <--mouse-size-px N> <command>\n\
                     commands: click drag get_screenshot move press_key scroll type_text"
                );
                std::process::exit(0);
            }
            other if other.starts_with("--") => {
                // Tolerate unknown "--key value" options.
                if args.peek().map(|v| !v.starts_with("--")).unwrap_or(false) {
                    let _ = args.next();
                }
            }
            other => {
                command = Some(other.to_string());
                break;
            }
        }
    }
    let command = command.context("no subcommand provided")?;
    Ok(Cli {
        command,
        timeout_ms,
    })
}

fn read_stdin_json() -> Result<Value> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read stdin")?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(trimmed).context("failed to parse json")
}

fn main() {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("__daemon") {
        // Daemon process: never returns until the session ends.
        if let Err(e) = run_daemon() {
            if let Ok(path) = status_path() {
                let _ = std::fs::write(path, format!("error: {e:#}"));
            }
            eprintln!("sky_wayland daemon error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(e) = run_client() {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn run_client() -> Result<()> {
    let cli = parse_cli()?;
    let input = read_stdin_json()?;

    let request =
        serde_json::json!({ "cmd": cli.command, "input": input, "timeout_ms": cli.timeout_ms });
    let response = send_to_daemon(&request, cli.timeout_ms).await?;

    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        if let Some(out) = response.get("stdout").and_then(Value::as_str) {
            if !out.is_empty() {
                print!("{out}");
            }
        }
        Ok(())
    } else {
        let err = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown daemon error");
        bail!("{err}")
    }
}

async fn send_to_daemon(request: &Value, timeout_ms: u64) -> Result<Value> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::time::timeout;

    let path = socket_path()?;
    let request_timeout = Duration::from_millis(timeout_ms);

    // Fast path: daemon already running.
    if let Ok(Ok(stream)) = timeout(request_timeout, UnixStream::connect(&path)).await {
        return timeout(request_timeout, round_trip(stream, request))
            .await
            .context("timed out waiting for the computer-use helper response")?;
    }

    ensure_daemon_started().await?;

    // Wait for the daemon to come up. The first launch shows a one-time portal
    // permission dialog, so allow a generous window for the user to accept it.
    let startup_timeout = Duration::from_millis(timeout_ms.max(FIRST_START_TIMEOUT_MS));
    let deadline = std::time::Instant::now() + startup_timeout;
    loop {
        match timeout(request_timeout, UnixStream::connect(&path)).await {
            Ok(Ok(stream)) => {
                return timeout(request_timeout, round_trip(stream, request))
                    .await
                    .context("timed out waiting for the computer-use helper response")?;
            }
            Err(_) => {
                bail!("timed out connecting to the computer-use helper");
            }
            Ok(Err(_)) => {
                if let Ok(status) = status_path()
                    .and_then(|path| std::fs::read_to_string(path).context("read helper status"))
                {
                    if let Some(rest) = status.strip_prefix("error: ") {
                        bail!("computer-use portal setup failed: {}", rest.trim());
                    }
                }
                if std::time::Instant::now() > deadline {
                    bail!("timed out waiting for the computer-use helper to become ready");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }

    async fn round_trip(stream: UnixStream, request: &Value) -> Result<Value> {
        let mut reader = BufReader::new(stream);
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        reader.get_mut().write_all(line.as_bytes()).await?;
        reader.get_mut().flush().await?;
        let mut resp = String::new();
        reader.read_line(&mut resp).await?;
        if resp.trim().is_empty() {
            bail!("empty response from computer-use helper");
        }
        Ok(serde_json::from_str(resp.trim())?)
    }
}

async fn ensure_daemon_started() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let _ = std::fs::remove_file(status_path()?);
    let exe = std::env::current_exe().context("current_exe")?;
    // Detach into its own session so it outlives this short-lived client.
    unsafe {
        std::process::Command::new(exe)
            .arg("__daemon")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(|| {
                libc::setsid();
                Ok(())
            })
            .spawn()
            .context("failed to spawn computer-use helper daemon")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Action input types (subset of the FullDesktop.* contract)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ClickInput {
    x: f64,
    y: f64,
    expected_window_uuid: String,
    mouse_button: Option<String>,
    click_count: Option<u32>,
    key: Option<String>,
}

#[derive(Deserialize)]
struct DragInput {
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    expected_window_uuid: String,
    key: Option<String>,
}

#[derive(Deserialize)]
struct MoveInput {
    x: f64,
    y: f64,
    expected_window_uuid: String,
    key: Option<String>,
}

#[derive(Deserialize)]
struct ScrollInput {
    direction: String,
    expected_window_uuid: String,
    pixels: Option<f64>,
    x: Option<f64>,
    y: Option<f64>,
    key: Option<String>,
}

#[derive(Deserialize)]
struct PressKeyInput {
    key: String,
    expected_window_uuid: String,
}

#[derive(Deserialize)]
struct TypeTextInput {
    text: String,
    expected_window_uuid: String,
}

fn parse_mouse_button(s: Option<&str>) -> Result<i32> {
    match s {
        None | Some("left") | Some("l") => Ok(BTN_LEFT),
        Some("right") | Some("r") => Ok(BTN_RIGHT),
        Some("middle") | Some("m") => Ok(BTN_MIDDLE),
        Some(other) => bail!("invalid mouse button: {other}"),
    }
}

fn parse_click_count(count: Option<u32>) -> Result<u32> {
    let count = count.unwrap_or(1);
    if !(1..=3).contains(&count) {
        bail!("click_count must be between 1 and 3");
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};

struct Backend {
    remote: RemoteDesktop,
    screencast: Screencast,
    session: Session<RemoteDesktop>,
    kwin: kwin::KWinBackend,
    node_id: u32,
    origin_x: f64,
    origin_y: f64,
    width: f64,
    height: f64,
    invalid: AtomicBool,
}

#[tokio::main(flavor = "current_thread")]
async fn run_daemon() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    // If another daemon already owns the socket, exit quietly.
    let path = socket_path()?;
    if tokio::net::UnixStream::connect(&path).await.is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_file(&path);

    let backend = Backend::new().await?;
    let _ = std::fs::write(status_path()?, "ready");

    let listener = UnixListener::bind(&path).context("bind control socket")?;

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let bytes_read = match tokio::time::timeout(
            Duration::from_millis(MAX_TIMEOUT_MS),
            reader.read_line(&mut line),
        )
        .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) | Err(_) => continue,
        };
        if bytes_read == 0 {
            continue;
        }
        let response = match serde_json::from_str::<Value>(line.trim()) {
            Ok(req) => backend.handle(&req).await,
            Err(e) => serde_json::json!({ "ok": false, "error": format!("bad request json: {e}") }),
        };
        let mut out = serde_json::to_string(&response).unwrap_or_else(|_| {
            "{\"ok\":false,\"error\":\"failed to serialize response\"}".to_string()
        });
        out.push('\n');
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            reader.get_mut().write_all(out.as_bytes()).await?;
            reader.get_mut().flush().await
        })
        .await;
        if backend.is_invalid() {
            drop(listener);
            let _ = std::fs::remove_file(&path);
            if let Ok(status) = status_path() {
                let _ = std::fs::remove_file(status);
            }
            return Ok(());
        }
    }
}

impl Backend {
    async fn new() -> Result<Self> {
        let remote = RemoteDesktop::new().await.context("RemoteDesktop portal")?;
        let screencast = Screencast::new().await.context("ScreenCast portal")?;
        let session = remote
            .create_session(Default::default())
            .await
            .context("create_session")?;

        remote
            .select_devices(
                &session,
                ashpd::desktop::remote_desktop::SelectDevicesOptions::default()
                    .set_devices(DeviceType::Keyboard | DeviceType::Pointer),
            )
            .await
            .context("select_devices")?;

        screencast
            .select_sources(
                &session,
                ashpd::desktop::screencast::SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(Some(SourceType::Monitor.into()))
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .context("select_sources")?;

        let response = remote
            .start(&session, None, Default::default())
            .await
            .context("start session")?
            .response()
            .context("start session response (permission denied?)")?;

        let streams = response.streams();
        if streams.len() != 1 {
            bail!(
                "expected exactly one monitor screencast stream, received {}",
                streams.len()
            );
        }
        let stream = streams[0].clone();
        let node_id = stream.pipe_wire_node_id();
        let (origin_x, origin_y) = stream
            .position()
            .map(|(x, y)| (x as f64, y as f64))
            .ok_or_else(|| anyhow!("screencast stream did not report a compositor position"))?;
        let (width, height) = stream
            .size()
            .map(|(w, h)| (w as f64, h as f64))
            .ok_or_else(|| anyhow!("screencast stream did not report a size"))?;
        if width <= 0.0 || height <= 0.0 {
            bail!("screencast stream reported nonpositive logical size {width}x{height}");
        }
        let kwin = kwin::KWinBackend::new()
            .await
            .context("initialize KWin focus verifier")?;

        Ok(Backend {
            remote,
            screencast,
            session,
            kwin,
            node_id,
            origin_x,
            origin_y,
            width,
            height,
            invalid: AtomicBool::new(false),
        })
    }

    async fn handle(&self, req: &Value) -> Value {
        if let Err(error) = self.ensure_valid() {
            return serde_json::json!({ "ok": false, "error": format!("{error:#}") });
        }
        let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
        let input = req.get("input").cloned().unwrap_or(Value::Null);
        let timeout_ms = match request_timeout_ms(req) {
            Ok(value) => value,
            Err(error) => {
                return serde_json::json!({ "ok": false, "error": format!("{error:#}") });
            }
        };
        let (deadline, cleanup_deadline) = Deadline::for_request(timeout_ms);
        // Never wrap dispatch itself in timeout: every awaited operation uses
        // the deadline, and dispatch must retain ownership of cleanup state.
        let operation = self.dispatch(cmd, input, deadline, cleanup_deadline).await;
        let result = if self.is_invalid() {
            let close = self.close_invalid_session(cleanup_deadline).await;
            combine_string_operation_cleanup(operation, close)
        } else {
            operation
        };
        match result {
            Ok(stdout) => serde_json::json!({ "ok": true, "stdout": stdout }),
            Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
        }
    }

    async fn dispatch(
        &self,
        cmd: &str,
        input: Value,
        deadline: Deadline,
        cleanup_deadline: Deadline,
    ) -> Result<String> {
        match cmd {
            "get_screenshot" => self.get_screenshot(deadline).await,
            "move" => {
                let a: MoveInput = serde_json::from_value(input)?;
                validate_expected_uuid(&a.expected_window_uuid)?;
                self.local_point(a.x, a.y)?;
                let mut mods = self
                    .begin_mods(&a.key, &a.expected_window_uuid, deadline, cleanup_deadline)
                    .await?;
                let operation = async {
                    self.verify_focus(&a.expected_window_uuid, deadline).await?;
                    self.move_to(a.x, a.y, deadline).await
                }
                .await;
                let cleanup = self.release_keys(&mut mods, cleanup_deadline).await;
                combine_operation_cleanup(operation, cleanup)?;
                Ok(String::new())
            }
            "click" => {
                let a: ClickInput = serde_json::from_value(input)?;
                validate_expected_uuid(&a.expected_window_uuid)?;
                self.local_point(a.x, a.y)?;
                let button = parse_mouse_button(a.mouse_button.as_deref())?;
                let count = parse_click_count(a.click_count)?;
                let mut mods = self
                    .begin_mods(&a.key, &a.expected_window_uuid, deadline, cleanup_deadline)
                    .await?;
                let mut pressed_buttons = Vec::new();
                let operation = async {
                    self.verify_focus(&a.expected_window_uuid, deadline).await?;
                    self.move_to(a.x, a.y, deadline).await?;
                    for _ in 0..count {
                        deadline.check()?;
                        self.verify_focus(&a.expected_window_uuid, deadline).await?;
                        if let Err(error) = self.button(button, true, deadline).await {
                            if self.is_invalid() {
                                pressed_buttons.push(button);
                            }
                            return Err(error);
                        }
                        pressed_buttons.push(button);
                        let release = self.button(button, false, deadline).await;
                        if release.is_ok() {
                            pressed_buttons.pop();
                        }
                        release?;
                    }
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                let button_cleanup = self
                    .release_buttons(&mut pressed_buttons, cleanup_deadline)
                    .await;
                let modifier_cleanup = self.release_keys(&mut mods, cleanup_deadline).await;
                let cleanup = combine_cleanups(button_cleanup, modifier_cleanup);
                combine_operation_cleanup(operation, cleanup)?;
                Ok(String::new())
            }
            "drag" => {
                let a: DragInput = serde_json::from_value(input)?;
                validate_expected_uuid(&a.expected_window_uuid)?;
                self.local_point(a.from_x, a.from_y)?;
                self.local_point(a.to_x, a.to_y)?;
                let mut mods = self
                    .begin_mods(&a.key, &a.expected_window_uuid, deadline, cleanup_deadline)
                    .await?;
                let mut pressed_buttons = Vec::new();
                let operation = async {
                    self.verify_focus(&a.expected_window_uuid, deadline).await?;
                    self.move_to(a.from_x, a.from_y, deadline).await?;
                    self.verify_focus(&a.expected_window_uuid, deadline).await?;
                    if let Err(error) = self.button(BTN_LEFT, true, deadline).await {
                        if self.is_invalid() {
                            pressed_buttons.push(BTN_LEFT);
                        }
                        return Err(error);
                    }
                    pressed_buttons.push(BTN_LEFT);
                    self.verify_focus(&a.expected_window_uuid, deadline).await?;
                    self.move_to(a.to_x, a.to_y, deadline).await?;
                    self.verify_focus(&a.expected_window_uuid, deadline).await?;
                    let release = self.button(BTN_LEFT, false, deadline).await;
                    if release.is_ok() {
                        pressed_buttons.pop();
                    }
                    release?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                let button_cleanup = self
                    .release_buttons(&mut pressed_buttons, cleanup_deadline)
                    .await;
                let modifier_cleanup = self.release_keys(&mut mods, cleanup_deadline).await;
                let cleanup = combine_cleanups(button_cleanup, modifier_cleanup);
                combine_operation_cleanup(operation, cleanup)?;
                Ok(String::new())
            }
            "scroll" => {
                let a: ScrollInput = serde_json::from_value(input)?;
                validate_expected_uuid(&a.expected_window_uuid)?;
                let point = match (a.x, a.y) {
                    (Some(x), Some(y)) => {
                        self.local_point(x, y)?;
                        Some((x, y))
                    }
                    (None, None) => None,
                    _ => bail!("scroll x and y must be provided together"),
                };
                validate_scroll(&a.direction, a.pixels)?;
                let mut mods = self
                    .begin_mods(&a.key, &a.expected_window_uuid, deadline, cleanup_deadline)
                    .await?;
                let operation = async {
                    if let Some((x, y)) = point {
                        self.verify_focus(&a.expected_window_uuid, deadline).await?;
                        self.move_to(x, y, deadline).await?;
                    }
                    self.scroll(&a.direction, a.pixels, &a.expected_window_uuid, deadline)
                        .await
                }
                .await;
                let cleanup = self.release_keys(&mut mods, cleanup_deadline).await;
                combine_operation_cleanup(operation, cleanup)?;
                Ok(String::new())
            }
            "press_key" => {
                let a: PressKeyInput = serde_json::from_value(input)?;
                validate_expected_uuid(&a.expected_window_uuid)?;
                let chord = keymap::parse_chord(&a.key)?;
                let mut pressed = Vec::new();
                let operation = async {
                    for &ks in &chord {
                        deadline.check()?;
                        self.verify_focus(&a.expected_window_uuid, deadline).await?;
                        if let Err(error) = self.key(ks, true, deadline).await {
                            if self.is_invalid() {
                                pressed.push(ks);
                            }
                            return Err(error);
                        }
                        pressed.push(ks);
                    }
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if operation.is_ok() {
                    for &ks in chord.iter().rev() {
                        let release = self.key(ks, false, deadline).await;
                        if release.is_ok() {
                            pressed.pop();
                        }
                        if let Err(error) = release {
                            let cleanup = self.release_keys(&mut pressed, cleanup_deadline).await;
                            combine_operation_cleanup(Err(error), cleanup)?;
                        }
                    }
                }
                let cleanup = self.release_keys(&mut pressed, cleanup_deadline).await;
                combine_operation_cleanup(operation, cleanup)?;
                Ok(String::new())
            }
            "type_text" => {
                let a: TypeTextInput = serde_json::from_value(input)?;
                validate_expected_uuid(&a.expected_window_uuid)?;
                for ch in a.text.chars() {
                    deadline.check()?;
                    let ks = keymap::char_to_keysym(ch);
                    let mut pressed = Vec::new();
                    let operation = async {
                        self.verify_focus(&a.expected_window_uuid, deadline).await?;
                        if let Err(error) = self.key(ks, true, deadline).await {
                            if self.is_invalid() {
                                pressed.push(ks);
                            }
                            return Err(error);
                        }
                        pressed.push(ks);
                        let release = self.key(ks, false, deadline).await;
                        if release.is_ok() {
                            pressed.pop();
                        }
                        release
                    }
                    .await;
                    let cleanup = self.release_keys(&mut pressed, cleanup_deadline).await;
                    combine_operation_cleanup(operation, cleanup)?;
                }
                Ok(String::new())
            }
            other => bail!("unknown command: {other}"),
        }
    }

    fn is_invalid(&self) -> bool {
        self.invalid.load(Ordering::Acquire)
    }

    fn ensure_valid(&self) -> Result<()> {
        ensure_valid_state(&self.invalid)
    }

    fn mark_invalid(&self) {
        self.invalid.store(true, Ordering::Release);
    }

    async fn close_invalid_session(&self, deadline: Deadline) -> Result<()> {
        // Even if release cleanup consumed its budget, poll Close at least
        // once before terminating; dropping the daemon connection then
        // provides the final authoritative portal-session teardown.
        let remaining = deadline
            .remaining()
            .unwrap_or(Duration::from_millis(FORCED_CLOSE_ATTEMPT_MS));
        match tokio::time::timeout(remaining, self.session.close()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error).context("failed to close invalid RemoteDesktop session"),
            Err(_) => bail!("timed out closing invalid RemoteDesktop session"),
        }
    }

    async fn verify_focus(&self, expected_uuid: &str, deadline: Deadline) -> Result<()> {
        self.ensure_valid()?;
        validate_expected_uuid(expected_uuid)?;
        let remaining = deadline.remaining()?;
        let actual = tokio::time::timeout(remaining, self.kwin.active_uuid())
            .await
            .context("request deadline expired while checking active window UUID")??;
        if actual.as_deref() != Some(expected_uuid) {
            bail!(
                "active window UUID mismatch: expected {expected_uuid:?}, actual {:?}",
                actual.as_deref().unwrap_or("<none>")
            );
        }
        Ok(())
    }

    async fn begin_mods(
        &self,
        key: &Option<String>,
        expected_uuid: &str,
        deadline: Deadline,
        cleanup_deadline: Deadline,
    ) -> Result<Vec<u32>> {
        let mods = match key {
            Some(spec) if !spec.trim().is_empty() => keymap::parse_chord(spec)?,
            _ => Vec::new(),
        };
        let mut pressed = Vec::new();
        for &ks in &mods {
            let press = async {
                deadline.check()?;
                self.verify_focus(expected_uuid, deadline).await?;
                self.key(ks, true, deadline).await
            }
            .await;
            if let Err(error) = press {
                if self.is_invalid() {
                    pressed.push(ks);
                }
                let cleanup = self.release_keys(&mut pressed, cleanup_deadline).await;
                return combine_operation_cleanup(Err(error), cleanup).map(|()| pressed);
            }
            pressed.push(ks);
        }
        Ok(pressed)
    }

    async fn release_keys(&self, pressed: &mut Vec<u32>, deadline: Deadline) -> Result<()> {
        let mut errors = Vec::new();
        for index in (0..pressed.len()).rev() {
            match self.key(pressed[index], false, deadline).await {
                Ok(()) => {
                    pressed.remove(index);
                }
                Err(error) => {
                    errors.push(format!("{error:#}"));
                }
            }
        }
        cleanup_result("key release", errors)
    }

    async fn release_buttons(&self, pressed: &mut Vec<i32>, deadline: Deadline) -> Result<()> {
        let mut errors = Vec::new();
        for index in (0..pressed.len()).rev() {
            match self.button(pressed[index], false, deadline).await {
                Ok(()) => {
                    pressed.remove(index);
                }
                Err(error) => {
                    errors.push(format!("{error:#}"));
                }
            }
        }
        cleanup_result("button release", errors)
    }

    /// Convert global compositor coordinates to coordinates local to the
    /// exactly-one monitor selected by the portal.
    fn local_point(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        local_point(x, y, self.origin_x, self.origin_y, self.width, self.height)
    }

    async fn move_to(&self, x: f64, y: f64, deadline: Deadline) -> Result<()> {
        self.ensure_valid()?;
        let (local_x, local_y) = self.local_point(x, y)?;
        let remaining = deadline.remaining()?;
        match tokio::time::timeout(
            remaining,
            self.remote.notify_pointer_motion_absolute(
                &self.session,
                self.node_id,
                local_x,
                local_y,
                Default::default(),
            ),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.mark_invalid();
                Err(error).context("pointer motion failed; RemoteDesktop session invalidated")
            }
            Err(_) => {
                self.mark_invalid();
                bail!("pointer motion timed out; RemoteDesktop session invalidated")
            }
        }
    }

    async fn button(&self, button: i32, pressed: bool, deadline: Deadline) -> Result<()> {
        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        let remaining = match deadline.remaining() {
            Ok(remaining) => remaining,
            Err(error) => {
                if !pressed {
                    self.mark_invalid();
                }
                return Err(error);
            }
        };
        match tokio::time::timeout(
            remaining,
            self.remote
                .notify_pointer_button(&self.session, button, state, Default::default()),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.mark_invalid();
                Err(error).context("pointer button failed; RemoteDesktop session invalidated")
            }
            Err(_) => {
                self.mark_invalid();
                bail!("pointer button timed out; RemoteDesktop session invalidated")
            }
        }
    }

    async fn scroll(
        &self,
        direction: &str,
        pixels: Option<f64>,
        expected_uuid: &str,
        deadline: Deadline,
    ) -> Result<()> {
        use ashpd::desktop::remote_desktop::Axis;
        validate_scroll(direction, pixels)?;
        let magnitude = pixels.unwrap_or(120.0).abs();
        let (axis, delta) = match direction.trim() {
            "up" | "u" => (Axis::Vertical, -magnitude),
            "down" | "d" => (Axis::Vertical, magnitude),
            "left" | "l" => (Axis::Horizontal, -magnitude),
            "right" | "r" => (Axis::Horizontal, magnitude),
            other => bail!("invalid scroll direction: {other}"),
        };
        let (dx, dy) = match axis {
            Axis::Vertical => (0.0, delta),
            Axis::Horizontal => (delta, 0.0),
        };
        self.verify_focus(expected_uuid, deadline).await?;
        let remaining = deadline.remaining()?;
        match tokio::time::timeout(
            remaining,
            self.remote
                .notify_pointer_axis(&self.session, dx, dy, Default::default()),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.mark_invalid();
                Err(error).context("pointer axis failed; RemoteDesktop session invalidated")
            }
            Err(_) => {
                self.mark_invalid();
                bail!("pointer axis timed out; RemoteDesktop session invalidated")
            }
        }
    }

    async fn key(&self, keysym: u32, pressed: bool, deadline: Deadline) -> Result<()> {
        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        let remaining = match deadline.remaining() {
            Ok(remaining) => remaining,
            Err(error) => {
                if !pressed {
                    self.mark_invalid();
                }
                return Err(error);
            }
        };
        match tokio::time::timeout(
            remaining,
            self.remote.notify_keyboard_keysym(
                &self.session,
                keysym as i32,
                state,
                Default::default(),
            ),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.mark_invalid();
                Err(error).context("keyboard keysym failed; RemoteDesktop session invalidated")
            }
            Err(_) => {
                self.mark_invalid();
                bail!("keyboard keysym timed out; RemoteDesktop session invalidated")
            }
        }
    }

    async fn get_screenshot(&self, deadline: Deadline) -> Result<String> {
        self.ensure_valid()?;
        let remaining = deadline.remaining()?;
        let fd = match tokio::time::timeout(
            remaining,
            self.screencast
                .open_pipe_wire_remote(&self.session, Default::default()),
        )
        .await
        {
            Ok(Ok(fd)) => fd,
            Ok(Err(error)) => {
                self.mark_invalid();
                return Err(error)
                    .context("open_pipe_wire_remote failed; RemoteDesktop session invalidated");
            }
            Err(_) => {
                self.mark_invalid();
                bail!("open_pipe_wire_remote timed out; RemoteDesktop session invalidated");
            }
        };

        let dir = std::env::temp_dir();
        let filepath = dir.join(format!(
            "codex-cu-{}-{}.jpg",
            std::process::id(),
            now_millis()
        ));

        deadline.check()?;
        capture_frame_jpeg(fd, self.node_id, &filepath)?;
        deadline.check()?;

        let arr = serde_json::json!([{ "filepath": filepath.to_string_lossy() }]);
        Ok(serde_json::to_string(&arr)?)
    }
}

fn request_timeout_ms(req: &Value) -> Result<u64> {
    let timeout_ms = req
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .context("request timeout_ms must be an unsigned integer")?;
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        bail!("request timeout_ms must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS}");
    }
    Ok(timeout_ms)
}

fn validate_expected_uuid(expected_uuid: &str) -> Result<()> {
    if expected_uuid.trim().is_empty() {
        bail!("expected_window_uuid must not be empty");
    }
    Ok(())
}

fn local_point(
    x: f64,
    y: f64,
    origin_x: f64,
    origin_y: f64,
    width: f64,
    height: f64,
) -> Result<(f64, f64)> {
    if !x.is_finite() || !y.is_finite() {
        bail!("global compositor coordinates must be finite");
    }
    if !origin_x.is_finite()
        || !origin_y.is_finite()
        || !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
    {
        bail!("selected monitor has invalid compositor geometry");
    }
    let end_x = origin_x + width;
    let end_y = origin_y + height;
    if !end_x.is_finite() || !end_y.is_finite() {
        bail!("selected monitor compositor bounds overflowed");
    }
    if x < origin_x || x >= end_x || y < origin_y || y >= end_y {
        bail!(
            "global point ({x},{y}) is outside selected monitor bounds \
             {origin_x}<=x<{end_x}, {origin_y}<=y<{end_y}"
        );
    }
    Ok((x - origin_x, y - origin_y))
}

fn validate_scroll(direction: &str, pixels: Option<f64>) -> Result<()> {
    if let Some(pixels) = pixels {
        if !pixels.is_finite() {
            bail!("scroll pixels must be finite");
        }
    }
    match direction.trim() {
        "up" | "u" | "down" | "d" | "left" | "l" | "right" | "r" => Ok(()),
        other => bail!("invalid scroll direction: {other}"),
    }
}

fn cleanup_result(kind: &str, errors: Vec<String>) -> Result<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{kind} cleanup failed: {}", errors.join("; "))
    }
}

fn ensure_valid_state(invalid: &AtomicBool) -> Result<()> {
    if invalid.load(Ordering::Acquire) {
        bail!("RemoteDesktop backend is invalid; daemon is terminating");
    }
    Ok(())
}

fn combine_cleanups(first: Result<()>, second: Result<()>) -> Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => {
            bail!("{first:#}; additional cleanup failure: {second:#}")
        }
    }
}

fn combine_string_operation_cleanup(
    operation: Result<String>,
    cleanup: Result<()>,
) -> Result<String> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => {
            bail!("{error:#}; session close also failed: {cleanup:#}")
        }
    }
}

fn combine_operation_cleanup(operation: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => {
            bail!("{error:#}; cleanup also failed: {cleanup:#}")
        }
    }
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Grab a single frame from the PipeWire node behind the portal fd and encode
/// it as JPEG, using the system GStreamer pipewire element.
fn capture_frame_jpeg(fd: OwnedFd, node_id: u32, out: &std::path::Path) -> Result<()> {
    use std::os::fd::IntoRawFd;

    // The child must inherit the pipewire remote fd, so clear CLOEXEC.
    let raw = fd.into_raw_fd();
    clear_cloexec(raw)?;

    let result = std::process::Command::new("gst-launch-1.0")
        .arg("-q")
        .arg("pipewiresrc")
        .arg(format!("fd={raw}"))
        .arg(format!("path={node_id}"))
        .arg("num-buffers=1")
        .arg("!")
        .arg("videoconvert")
        .arg("!")
        .arg("video/x-raw,format=I420")
        .arg("!")
        .arg("jpegenc")
        .arg("quality=80")
        .arg("!")
        .arg("filesink")
        .arg(format!("location={}", out.display()))
        .output()
        .context("failed to run gst-launch-1.0 (install gstreamer + gst-plugin-pipewire)");

    // Reap the fd now that the child is done with it.
    unsafe { libc::close(raw) };
    let output = result?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "gstreamer capture failed ({}): {}",
            output.status,
            stderr.trim()
        );
    }
    if !out.exists() {
        bail!("screenshot file was not produced");
    }
    Ok(())
}

fn clear_cloexec(raw: std::os::fd::RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 {
        bail!("fcntl F_GETFD failed");
    }
    let res = unsafe { libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    if res < 0 {
        bail!("fcntl F_SETFD failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_global_points_across_monitor_origins() {
        assert_eq!(
            local_point(1920.0, 0.0, 1920.0, 0.0, 2560.0, 1440.0).unwrap(),
            (0.0, 0.0)
        );
        assert_eq!(
            local_point(-1279.0, 100.0, -1280.0, 0.0, 1280.0, 1024.0).unwrap(),
            (1.0, 100.0)
        );
        assert!(local_point(4480.0, 0.0, 1920.0, 0.0, 2560.0, 1440.0).is_err());
        assert!(local_point(-1281.0, 0.0, -1280.0, 0.0, 1280.0, 1024.0).is_err());
        assert!(local_point(f64::NAN, 0.0, 0.0, 0.0, 100.0, 100.0).is_err());
    }

    #[test]
    fn parses_only_supported_buttons_and_click_counts() {
        assert_eq!(parse_mouse_button(None).unwrap(), BTN_LEFT);
        assert_eq!(parse_mouse_button(Some("left")).unwrap(), BTN_LEFT);
        assert_eq!(parse_mouse_button(Some("l")).unwrap(), BTN_LEFT);
        assert_eq!(parse_mouse_button(Some("right")).unwrap(), BTN_RIGHT);
        assert_eq!(parse_mouse_button(Some("r")).unwrap(), BTN_RIGHT);
        assert_eq!(parse_mouse_button(Some("middle")).unwrap(), BTN_MIDDLE);
        assert_eq!(parse_mouse_button(Some("m")).unwrap(), BTN_MIDDLE);
        assert!(parse_mouse_button(Some("primary")).is_err());

        assert_eq!(parse_click_count(None).unwrap(), 1);
        assert_eq!(parse_click_count(Some(1)).unwrap(), 1);
        assert_eq!(parse_click_count(Some(3)).unwrap(), 3);
        assert!(parse_click_count(Some(0)).is_err());
        assert!(parse_click_count(Some(4)).is_err());
    }

    #[test]
    fn parses_and_bounds_timeout() {
        let default = parse_cli_from(["move".to_owned()]).unwrap();
        assert_eq!(default.timeout_ms, DEFAULT_TIMEOUT_MS);

        let minimum = parse_cli_from([
            "--timeout-ms".to_owned(),
            MIN_TIMEOUT_MS.to_string(),
            "click".to_owned(),
        ])
        .unwrap();
        assert_eq!(minimum.timeout_ms, MIN_TIMEOUT_MS);

        let maximum = parse_cli_from([
            "--timeout-ms".to_owned(),
            MAX_TIMEOUT_MS.to_string(),
            "drag".to_owned(),
        ])
        .unwrap();
        assert_eq!(maximum.timeout_ms, MAX_TIMEOUT_MS);

        assert!(parse_cli_from([
            "--timeout-ms".to_owned(),
            (MIN_TIMEOUT_MS - 1).to_string(),
            "move".to_owned(),
        ])
        .is_err());
        assert!(parse_cli_from([
            "--timeout-ms".to_owned(),
            (MAX_TIMEOUT_MS + 1).to_string(),
            "move".to_owned(),
        ])
        .is_err());
        assert!(parse_cli_from(["--timeout-ms".to_owned(), "not-a-number".to_owned()]).is_err());
    }

    #[test]
    fn deadline_reserves_cleanup_time_and_expires() {
        let now = std::time::Instant::now();
        let (deadline, cleanup) = Deadline::for_request_at(now, 1_000);
        assert_eq!(
            deadline.remaining_at(now).unwrap(),
            Duration::from_millis(900)
        );
        assert_eq!(
            cleanup.remaining_at(now).unwrap(),
            Duration::from_millis(980)
        );
        assert_eq!(
            cleanup
                .remaining_at(now + Duration::from_millis(900))
                .unwrap(),
            Duration::from_millis(80)
        );
        assert!(deadline
            .remaining_at(now + Duration::from_millis(900))
            .is_err());

        let (short, cleanup) = Deadline::for_request_at(now, MIN_TIMEOUT_MS);
        assert_eq!(short.remaining_at(now).unwrap(), Duration::from_millis(50));
        assert_eq!(
            cleanup.remaining_at(now).unwrap(),
            Duration::from_millis(80)
        );
    }

    #[test]
    fn invalid_state_gate_fails_closed() {
        let invalid = AtomicBool::new(false);
        assert!(ensure_valid_state(&invalid).is_ok());
        invalid.store(true, Ordering::Release);
        assert!(ensure_valid_state(&invalid).is_err());
    }
}
