//! Typed KWin 6 Wayland window management and capture backend.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd as StdOwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, timeout};
use zbus::zvariant::{OwnedFd, OwnedValue, Value};
use zbus::{Connection, Proxy};

const BRIDGE_PATH: &str = "/org/openai/CodexComputerUse/KWinBridge";
const BRIDGE_INTERFACE: &str = "org.openai.CodexComputerUse.KWinBridge";
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
const READER_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RAW_BYTES: usize = 256 * 1024 * 1024;
const BLANK_CAPTURE_RETRIES: usize = 2;
const BLANK_CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(120);

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowInfo {
    pub uuid: String,
    pub caption: String,
    pub resource_class: String,
    pub resource_name: String,
    pub desktop_file: String,
    pub pid: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub minimized: bool,
    pub fullscreen: bool,
}

pub struct WindowScreenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

#[derive(Clone, Debug)]
struct WindowRecord {
    info: WindowInfo,
    window_type: i32,
    skip_taskbar: bool,
}

#[derive(Debug)]
struct WindowNotFound(String);

impl std::fmt::Display for WindowNotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for WindowNotFound {}

pub fn is_window_not_found(error: &anyhow::Error) -> bool {
    error.downcast_ref::<WindowNotFound>().is_some()
}

pub fn exact_window_id(window: &WindowInfo) -> String {
    format!("{}@{}", app_identity(window), window.uuid)
}

#[derive(Debug, Deserialize)]
struct CompositorSnapshot {
    #[serde(default)]
    windows: Vec<String>,
    #[serde(default, rename = "activeWindow")]
    active_window: Option<String>,
}

struct SnapshotBridge {
    sender: mpsc::UnboundedSender<(i32, String)>,
}

#[zbus::interface(name = "org.openai.CodexComputerUse.KWinBridge")]
impl SnapshotBridge {
    #[zbus(name = "Snapshot")]
    fn snapshot(&self, request_id: i32, data: String) {
        let _ = self.sender.send((request_id, data));
    }
}

pub struct KWinBackend {
    connection: Connection,
    snapshot_lock: Mutex<()>,
    snapshot_receiver: Mutex<mpsc::UnboundedReceiver<(i32, String)>>,
}

impl KWinBackend {
    /// Connect to the session bus and verify the required KWin interfaces.
    pub async fn new() -> Result<Self> {
        let connection = Connection::session()
            .await
            .context("failed to connect to the D-Bus session bus")?;

        let kwin = proxy(&connection, "org.kde.KWin", "/KWin", "org.kde.KWin")
            .await
            .context("KWin window API is unavailable")?;
        timeout(
            CALL_TIMEOUT,
            kwin.call::<_, _, HashMap<String, OwnedValue>>(
                "getWindowInfo",
                &("__codex_preflight_nonexistent_window__",),
            ),
        )
        .await
        .context("timed out probing the KWin window API")?
        .context("KWin getWindowInfo preflight failed")?;

        let screenshot = proxy(
            &connection,
            "org.kde.KWin.ScreenShot2",
            "/org/kde/KWin/ScreenShot2",
            "org.kde.KWin.ScreenShot2",
        )
        .await
        .context("KWin ScreenShot2 is unavailable")?;
        let version = timeout(CALL_TIMEOUT, screenshot.get_property::<u32>("Version"))
            .await
            .context("timed out reading KWin ScreenShot2.Version")?
            .context("failed to read KWin ScreenShot2.Version")?;
        if version < 5 {
            bail!(
                "KWin ScreenShot2 version {version} is unsupported; version 5 or newer is required"
            );
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        let added = connection
            .object_server()
            .at(BRIDGE_PATH, SnapshotBridge { sender })
            .await
            .context("failed to register the KWin script callback bridge")?;
        if !added {
            bail!("KWin script callback bridge path is already registered");
        }

        Ok(Self {
            connection,
            snapshot_lock: Mutex::new(()),
            snapshot_receiver: Mutex::new(receiver),
        })
    }

    /// Enumerate normal task windows in compositor order.
    pub async fn windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(self
            .window_records()
            .await?
            .into_iter()
            .map(|record| record.info)
            .collect())
    }

    /// Return the compositor's active window UUID.
    pub async fn active_uuid(&self) -> Result<Option<String>> {
        Ok(self.compositor_snapshot().await?.active_window)
    }

    /// Resolve an application query to exactly one normal task window.
    pub async fn resolve_window(&self, app: &str) -> Result<WindowInfo> {
        let records = self.window_records().await?;
        resolve_records(&records, app)
    }

    /// Activate a window by exact UUID and verify compositor focus.
    pub async fn activate(&self, window: &WindowInfo) -> Result<()> {
        if !self
            .compositor_snapshot()
            .await?
            .windows
            .iter()
            .any(|uuid| uuid == &window.uuid)
        {
            bail!("cannot activate stale KWin window UUID {}", window.uuid);
        }

        let runner = proxy(
            &self.connection,
            "org.kde.KWin",
            "/WindowsRunner",
            "org.kde.krunner1",
        )
        .await?;
        let match_id = format!("0_{}", window.uuid);
        timeout(
            CALL_TIMEOUT,
            runner.call::<_, _, ()>("Run", &(match_id.as_str(), "")),
        )
        .await
        .context("timed out activating the KWin window")?
        .with_context(|| format!("KWin runner failed to activate UUID {}", window.uuid))?;

        for delay in [
            Duration::from_millis(0),
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(300),
        ] {
            sleep(delay).await;
            if self.active_uuid().await?.as_deref() == Some(window.uuid.as_str()) {
                return Ok(());
            }
        }
        bail!(
            "KWin did not activate window UUID {} within verification deadline",
            window.uuid
        )
    }

    /// Capture one exact KWin window, rejecting persistent blank compositor frames.
    pub async fn screenshot(&self, window: &WindowInfo) -> Result<WindowScreenshot> {
        if window.minimized {
            bail!("cannot capture minimized KWin window UUID {}", window.uuid);
        }
        for attempt in 0..=BLANK_CAPTURE_RETRIES {
            let (capture, rgba) = self.capture_frame(window).await?;
            if !frame_is_blank(&rgba) {
                return Ok(WindowScreenshot {
                    png: encode_png(capture.width, capture.height, &rgba)?,
                    width: capture.width,
                    height: capture.height,
                    scale: capture.scale,
                });
            }
            if attempt < BLANK_CAPTURE_RETRIES {
                sleep(BLANK_CAPTURE_RETRY_DELAY).await;
            }
        }
        bail!(
            "KWin returned {} consecutive blank captures for window {}; the compositor did not render a usable frame",
            BLANK_CAPTURE_RETRIES + 1,
            window.uuid
        )
    }

    async fn capture_frame(&self, window: &WindowInfo) -> Result<(CaptureMetadata, Vec<u8>)> {
        if !self
            .compositor_snapshot()
            .await?
            .windows
            .iter()
            .any(|uuid| uuid == &window.uuid)
        {
            bail!("cannot capture stale KWin window UUID {}", window.uuid);
        }

        let (read_fd, write_fd) = create_pipe()?;
        let dbus_fd = OwnedFd::from(write_fd);
        let options = HashMap::from([
            ("include-decoration", Value::from(true)),
            ("include-shadow", Value::from(false)),
            ("include-cursor", Value::from(false)),
        ]);
        let screenshot = proxy(
            &self.connection,
            "org.kde.KWin.ScreenShot2",
            "/org/kde/KWin/ScreenShot2",
            "org.kde.KWin.ScreenShot2",
        )
        .await?;

        let capture = async {
            match timeout(
                CAPTURE_TIMEOUT,
                screenshot.call::<_, _, HashMap<String, OwnedValue>>(
                    "CaptureWindow",
                    &(window.uuid.as_str(), options, dbus_fd),
                ),
            )
            .await
            {
                Err(_) => Err(anyhow!("timed out capturing KWin window {}", window.uuid)),
                Ok(Err(error)) => {
                    let message = error.to_string();
                    if message.contains("NoAuthorized") || message.contains("NotAuthorized") {
                        Err(anyhow!(
                            "KWin denied ScreenShot2 authorization; the desktop entry must declare \
                             X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2"
                        ))
                    } else {
                        Err(error).with_context(|| {
                            format!("failed to capture KWin window {}", window.uuid)
                        })
                    }
                }
                Ok(Ok(metadata)) => Ok(metadata),
            }
        };
        let reader = async {
            timeout(READER_TIMEOUT, read_pipe(read_fd))
                .await
                .context("timed out reading KWin screenshot pixels")?
        };
        let (capture_result, read_result) = tokio::join!(capture, reader);
        let metadata = capture_result?;
        let raw = read_result?;
        let capture = parse_capture_metadata(&metadata, &window.uuid)?;
        let rgba = bgra_to_rgba(
            &raw,
            capture.width,
            capture.height,
            capture.stride,
            capture.format,
        )?;
        Ok((capture, rgba))
    }

    async fn window_records(&self) -> Result<Vec<WindowRecord>> {
        let snapshot = self.compositor_snapshot().await?;
        let kwin = proxy(&self.connection, "org.kde.KWin", "/KWin", "org.kde.KWin").await?;
        let mut records = Vec::new();
        for uuid in snapshot.windows {
            let metadata = timeout(
                CALL_TIMEOUT,
                kwin.call::<_, _, HashMap<String, OwnedValue>>("getWindowInfo", &(uuid.as_str(),)),
            )
            .await
            .with_context(|| format!("timed out reading metadata for KWin window {uuid}"))?
            .with_context(|| format!("failed to read metadata for KWin window {uuid}"))?;
            if metadata.is_empty() {
                continue;
            }
            let record = parse_window_metadata(&metadata)
                .with_context(|| format!("invalid metadata for KWin window {uuid}"))?;
            if record.info.uuid != uuid {
                bail!(
                    "KWin returned UUID {} for requested window {uuid}",
                    record.info.uuid
                );
            }
            if record.window_type == 0
                && !record.skip_taskbar
                && record.info.width > 0.0
                && record.info.height > 0.0
            {
                records.push(record);
            }
        }
        Ok(records)
    }

    async fn compositor_snapshot(&self) -> Result<CompositorSnapshot> {
        let _snapshot_guard = self.snapshot_lock.lock().await;
        let request_number = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let request_id =
            i32::try_from(request_number & 0x7fff_ffff).context("snapshot request ID overflow")?;
        let plugin_name = format!(
            "codex_computer_use_{}_{}",
            std::process::id(),
            request_number
        );
        let script_path = runtime_script_path(&plugin_name)?;
        let destination = self
            .connection
            .unique_name()
            .context("session bus connection has no unique name")?
            .as_str();
        let script = snapshot_script(destination, request_id)?;
        let mut script_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&script_path)
            .with_context(|| format!("failed to create KWin script {}", script_path.display()))?;
        if let Err(error) = script_file.write_all(script.as_bytes()) {
            drop(script_file);
            let _ = std::fs::remove_file(&script_path);
            return Err(error)
                .with_context(|| format!("failed to write KWin script {}", script_path.display()));
        }
        drop(script_file);

        let result = self
            .run_snapshot_script(&script_path, &plugin_name, request_id)
            .await;
        let remove_result = std::fs::remove_file(&script_path)
            .with_context(|| format!("failed to remove KWin script {}", script_path.display()));
        match (result, remove_result) {
            (Ok(snapshot), Ok(())) => Ok(snapshot),
            (Err(operation), Err(cleanup)) => Err(anyhow!(
                "KWin snapshot operation failed: {operation:#}; temp-file cleanup also failed: {cleanup:#}"
            )),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    async fn run_snapshot_script(
        &self,
        script_path: &std::path::Path,
        plugin_name: &str,
        request_id: i32,
    ) -> Result<CompositorSnapshot> {
        let scripting = proxy(
            &self.connection,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )
        .await?;
        let path = script_path
            .to_str()
            .context("KWin script path is not valid UTF-8")?;
        let script_id = timeout(
            CALL_TIMEOUT,
            scripting.call::<_, _, i32>("loadScript", &(path, plugin_name)),
        )
        .await
        .context("timed out loading the KWin discovery script")?
        .context("failed to load the KWin discovery script")?;
        if script_id < 0 {
            bail!("KWin rejected the discovery script");
        }

        let result = self.run_loaded_snapshot_script(script_id, request_id).await;
        let unload = timeout(
            CALL_TIMEOUT,
            scripting.call::<_, _, bool>("unloadScript", &(plugin_name,)),
        )
        .await
        .context("timed out unloading the KWin discovery script")
        .and_then(|value| value.context("failed to unload the KWin discovery script"))
        .and_then(|unloaded| {
            if unloaded {
                Ok(())
            } else {
                Err(anyhow!("KWin refused to unload the discovery script"))
            }
        });
        match (result, unload) {
            (Ok(snapshot), Ok(())) => Ok(snapshot),
            (Err(operation), Err(cleanup)) => Err(anyhow!(
                "KWin discovery script operation failed: {operation:#}; unloading also failed: {cleanup:#}"
            )),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    async fn run_loaded_snapshot_script(
        &self,
        script_id: i32,
        request_id: i32,
    ) -> Result<CompositorSnapshot> {
        let path = format!("/Scripting/Script{script_id}");
        let script = proxy(
            &self.connection,
            "org.kde.KWin",
            &path,
            "org.kde.kwin.Script",
        )
        .await?;
        let mut receiver = self.snapshot_receiver.lock().await;
        while receiver.try_recv().is_ok() {}
        timeout(CALL_TIMEOUT, script.call::<_, _, ()>("run", &()))
            .await
            .context("timed out starting the KWin discovery script")?
            .context("failed to start the KWin discovery script")?;

        let data = timeout(SNAPSHOT_TIMEOUT, async {
            loop {
                let (received_id, data) = receiver
                    .recv()
                    .await
                    .context("KWin snapshot callback bridge closed")?;
                if received_id == request_id {
                    return Ok::<_, anyhow::Error>(data);
                }
            }
        })
        .await
        .context("timed out waiting for the KWin discovery callback")??;
        serde_json::from_str(&data).context("KWin discovery callback returned invalid JSON")
    }
}

async fn proxy<'a>(
    connection: &'a Connection,
    destination: &'a str,
    path: &'a str,
    interface: &'a str,
) -> Result<Proxy<'a>> {
    Proxy::new(connection, destination, path, interface)
        .await
        .map_err(Into::into)
}

fn runtime_script_path(plugin_name: &str) -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is unset; refusing to create a KWin script elsewhere")?;
    let metadata = std::fs::metadata(&runtime).context("failed to inspect XDG_RUNTIME_DIR")?;
    if !metadata.is_dir() {
        bail!("XDG_RUNTIME_DIR is not a directory");
    }
    Ok(PathBuf::from(runtime).join(format!("{plugin_name}.js")))
}

fn snapshot_script(destination: &str, request_id: i32) -> Result<String> {
    let destination = serde_json::to_string(destination)?;
    let bridge_path = serde_json::to_string(BRIDGE_PATH)?;
    let bridge_interface = serde_json::to_string(BRIDGE_INTERFACE)?;
    Ok(format!(
        "const snapshot = {{\n\
         windows: workspace.windowList().map(w => w.internalId.toString()),\n\
         activeWindow: workspace.activeWindow ? workspace.activeWindow.internalId.toString() : null\n\
         }};\n\
         callDBus({destination}, {bridge_path}, {bridge_interface}, \"Snapshot\", \
         {request_id}, JSON.stringify(snapshot));\n"
    ))
}

fn parse_window_metadata(metadata: &HashMap<String, OwnedValue>) -> Result<WindowRecord> {
    let uuid = required_string(metadata, "uuid")?;
    if uuid.trim().is_empty() {
        bail!("uuid is empty");
    }
    let pid_i32 = required_i32(metadata, "pid")?;
    let pid = u32::try_from(pid_i32).context("pid is negative")?;
    Ok(WindowRecord {
        info: WindowInfo {
            uuid,
            caption: required_string(metadata, "caption")?,
            resource_class: required_string(metadata, "resourceClass")?,
            resource_name: required_string(metadata, "resourceName")?,
            desktop_file: required_string(metadata, "desktopFile")?,
            pid,
            x: required_f64(metadata, "x")?,
            y: required_f64(metadata, "y")?,
            width: required_f64(metadata, "width")?,
            height: required_f64(metadata, "height")?,
            minimized: required_bool(metadata, "minimized")?,
            fullscreen: required_bool(metadata, "fullscreen")?,
        },
        window_type: required_i32(metadata, "type")?,
        skip_taskbar: required_bool(metadata, "skipTaskbar")?,
    })
}

fn required_value<'a>(
    metadata: &'a HashMap<String, OwnedValue>,
    key: &str,
) -> Result<&'a OwnedValue> {
    metadata
        .get(key)
        .with_context(|| format!("missing metadata field {key}"))
}

fn required_string(metadata: &HashMap<String, OwnedValue>, key: &str) -> Result<String> {
    Ok(<&str>::try_from(required_value(metadata, key)?)
        .with_context(|| format!("metadata field {key} is not a string"))?
        .to_owned())
}

fn required_i32(metadata: &HashMap<String, OwnedValue>, key: &str) -> Result<i32> {
    i32::try_from(required_value(metadata, key)?)
        .with_context(|| format!("metadata field {key} is not int32"))
}

fn required_u32(metadata: &HashMap<String, OwnedValue>, key: &str) -> Result<u32> {
    u32::try_from(required_value(metadata, key)?)
        .with_context(|| format!("metadata field {key} is not uint32"))
}

fn required_f64(metadata: &HashMap<String, OwnedValue>, key: &str) -> Result<f64> {
    f64::try_from(required_value(metadata, key)?)
        .with_context(|| format!("metadata field {key} is not double"))
}

fn required_bool(metadata: &HashMap<String, OwnedValue>, key: &str) -> Result<bool> {
    bool::try_from(required_value(metadata, key)?)
        .with_context(|| format!("metadata field {key} is not boolean"))
}

fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

fn desktop_identity(value: &str) -> String {
    let normalized = normalize(value);
    normalized
        .strip_suffix(".desktop")
        .unwrap_or(&normalized)
        .to_owned()
}

fn app_identity(window: &WindowInfo) -> String {
    if window.desktop_file.trim().is_empty() {
        normalize(&window.resource_class)
    } else {
        desktop_identity(&window.desktop_file)
    }
}

fn score(window: &WindowInfo, query: &str) -> Option<u8> {
    let desktop = desktop_identity(&window.desktop_file);
    let class = normalize(&window.resource_class);
    let name = normalize(&window.resource_name);
    let caption = normalize(&window.caption);
    if (!desktop.is_empty() && desktop == query)
        || (!class.is_empty() && desktop_identity(&class) == query)
        || (!name.is_empty() && desktop_identity(&name) == query)
        || (!caption.is_empty() && desktop_identity(&caption) == query)
    {
        Some(100)
    } else {
        None
    }
}

fn resolve_records(records: &[WindowRecord], app: &str) -> Result<WindowInfo> {
    if let Some((identity, uuid)) = parse_exact_window_id(app)? {
        let record = records
            .iter()
            .find(|record| record.info.uuid == uuid)
            .ok_or_else(|| {
                anyhow!(
                "exact window ID {app:?} is stale; UUID {uuid} is not present in fresh KWin records"
            )
            })?;
        let actual_identity = app_identity(&record.info);
        if desktop_identity(identity) != desktop_identity(&actual_identity) {
            bail!(
                "exact window ID {app:?} identity mismatch: UUID {uuid} currently belongs to {actual_identity:?}"
            );
        }
        return Ok(record.info.clone());
    }
    let query = desktop_identity(app);
    if query.is_empty() || query == "unnamed" {
        bail!("application query must be non-empty and cannot be \"Unnamed\"");
    }
    let mut scored = records
        .iter()
        .filter_map(|record| score(&record.info, &query).map(|score| (record, score)))
        .collect::<Vec<_>>();
    let Some(best_score) = scored.iter().map(|(_, score)| *score).max() else {
        let available = records
            .iter()
            .map(|record| format!("{} ({:?})", app_identity(&record.info), record.info.caption))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(WindowNotFound(format!(
            "application {app:?} not found; available windows: {available}"
        ))
        .into());
    };
    scored.retain(|(_, score)| *score == best_score);
    if scored.len() == 1 {
        return Ok(scored[0].0.info.clone());
    }

    let candidates = scored
        .iter()
        .map(|(record, _)| exact_window_id(&record.info))
        .collect::<Vec<_>>()
        .join(", ");
    bail!("application {app:?} is ambiguous; candidates: {candidates}")
}

fn parse_exact_window_id(value: &str) -> Result<Option<(&str, &str)>> {
    let Some((identity, uuid)) = value.rsplit_once('@') else {
        return Ok(None);
    };
    if !uuid.starts_with('{') || !uuid.ends_with('}') {
        return Ok(None);
    }
    if identity.trim().is_empty() || uuid.len() <= 2 {
        bail!("invalid exact window ID {value:?}; expected <identity>@{{<uuid>}}");
    }
    Ok(Some((identity, uuid)))
}

struct CaptureMetadata {
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
    scale: f64,
}

fn parse_capture_metadata(
    metadata: &HashMap<String, OwnedValue>,
    expected_uuid: &str,
) -> Result<CaptureMetadata> {
    let capture_type = required_string(metadata, "type")?;
    if capture_type != "raw" {
        bail!("KWin returned unsupported screenshot type {capture_type:?}");
    }
    let window_id = required_string(metadata, "windowId")?;
    if window_id != expected_uuid {
        bail!("KWin screenshot UUID mismatch: expected {expected_uuid}, received {window_id}");
    }
    let format = required_u32(metadata, "format")?;
    if !matches!(format, 5 | 6) {
        bail!("unsupported KWin QImage format {format}; expected ARGB32 or ARGB32_Premultiplied");
    }
    let width = required_u32(metadata, "width")?;
    let height = required_u32(metadata, "height")?;
    let stride = required_u32(metadata, "stride")?;
    let scale = required_f64(metadata, "scale")?;
    if width == 0 || height == 0 || stride == 0 || !scale.is_finite() || scale <= 0.0 {
        bail!("KWin returned invalid screenshot dimensions or scale");
    }
    let row_bytes = width
        .checked_mul(4)
        .context("KWin screenshot row size overflow")?;
    if stride < row_bytes {
        bail!("KWin screenshot stride {stride} is smaller than row size {row_bytes}");
    }
    let byte_count = usize::try_from(stride)
        .ok()
        .and_then(|value| value.checked_mul(usize::try_from(height).ok()?))
        .context("KWin screenshot allocation overflow")?;
    if byte_count > MAX_RAW_BYTES {
        bail!("KWin screenshot exceeds the {MAX_RAW_BYTES}-byte safety limit");
    }
    Ok(CaptureMetadata {
        format,
        width,
        height,
        stride,
        scale,
    })
}

fn create_pipe() -> Result<(StdOwnedFd, StdOwnedFd)> {
    let mut fds = [0_i32; 2];
    // SAFETY: `fds` points to storage for exactly two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to create screenshot pipe");
    }
    // Keep KWin's transferred write descriptor blocking. Only the local read
    // end must be nonblocking for Tokio's AsyncFd readiness loop.
    let read_flags = unsafe { libc::fcntl(fds[0], libc::F_GETFL) };
    if read_flags < 0
        || unsafe { libc::fcntl(fds[0], libc::F_SETFL, read_flags | libc::O_NONBLOCK) } < 0
    {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return Err(error).context("failed to make screenshot read pipe nonblocking");
    }
    // SAFETY: successful pipe2 returned two newly-owned descriptors.
    Ok(unsafe {
        (
            StdOwnedFd::from_raw_fd(fds[0]),
            StdOwnedFd::from_raw_fd(fds[1]),
        )
    })
}

async fn read_pipe(fd: StdOwnedFd) -> Result<Vec<u8>> {
    let fd = AsyncFd::new(fd).context("failed to register KWin screenshot pipe with Tokio")?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let mut ready = fd
            .readable()
            .await
            .context("failed waiting for KWin screenshot pipe readiness")?;
        loop {
            // SAFETY: the descriptor remains owned by `fd`; buffer is valid writable storage.
            let count = unsafe {
                libc::read(
                    fd.get_ref().as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count > 0 {
                let count = usize::try_from(count).context("negative screenshot read count")?;
                if bytes.len().saturating_add(count) > MAX_RAW_BYTES {
                    bail!("KWin screenshot exceeds the {MAX_RAW_BYTES}-byte safety limit");
                }
                bytes.extend_from_slice(&buffer[..count]);
                continue;
            }
            if count == 0 {
                return Ok(bytes);
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    ready.clear_ready();
                    break;
                }
                _ => return Err(error).context("failed to read KWin screenshot pipe"),
            }
        }
    }
}

fn bgra_to_rgba(raw: &[u8], width: u32, height: u32, stride: u32, format: u32) -> Result<Vec<u8>> {
    let expected = usize::try_from(stride)
        .ok()
        .and_then(|value| value.checked_mul(usize::try_from(height).ok()?))
        .context("raw screenshot length overflow")?;
    if raw.len() != expected {
        bail!(
            "KWin screenshot byte count mismatch: expected {expected}, received {}",
            raw.len()
        );
    }
    let pixel_count = usize::try_from(width)
        .ok()
        .and_then(|value| value.checked_mul(usize::try_from(height).ok()?))
        .context("screenshot pixel count overflow")?;
    let mut rgba = Vec::with_capacity(
        pixel_count
            .checked_mul(4)
            .context("RGBA allocation overflow")?,
    );
    let row_pixels = usize::try_from(width).context("screenshot width overflow")?;
    let stride = usize::try_from(stride).context("screenshot stride overflow")?;
    for row in raw.chunks_exact(stride) {
        for pixel in row[..row_pixels * 4].chunks_exact(4) {
            let (mut red, mut green, mut blue, alpha) = (pixel[2], pixel[1], pixel[0], pixel[3]);
            if format == 6 && (1..=254).contains(&alpha) {
                red = unpremultiply(red, alpha);
                green = unpremultiply(green, alpha);
                blue = unpremultiply(blue, alpha);
            }
            rgba.extend_from_slice(&[red, green, blue, alpha]);
        }
    }
    Ok(rgba)
}

fn unpremultiply(channel: u8, alpha: u8) -> u8 {
    let value = (u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha);
    value.min(255) as u8
}

fn frame_is_blank(rgba: &[u8]) -> bool {
    let mut pixels = 0_usize;
    let mut dark = 0_usize;
    for pixel in rgba.chunks_exact(4) {
        pixels += 1;
        if pixel[3] == 0 || (pixel[0] <= 8 && pixel[1] <= 8 && pixel[2] <= 8) {
            dark += 1;
        }
    }
    pixels > 0 && dark * 1000 >= pixels * 995
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("failed to create PNG header")?;
        writer
            .write_image_data(rgba)
            .context("failed to encode screenshot PNG")?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(
        uuid: &str,
        desktop_file: &str,
        class: &str,
        name: &str,
        caption: &str,
        minimized: bool,
        geometry: (f64, f64),
    ) -> WindowRecord {
        WindowRecord {
            info: WindowInfo {
                uuid: uuid.to_owned(),
                caption: caption.to_owned(),
                resource_class: class.to_owned(),
                resource_name: name.to_owned(),
                desktop_file: desktop_file.to_owned(),
                pid: 42,
                x: 10.0,
                y: 20.0,
                width: geometry.0,
                height: geometry.1,
                minimized,
                fullscreen: false,
            },
            window_type: 0,
            skip_taskbar: false,
        }
    }

    #[test]
    fn resolution_prefers_exact_desktop_identity() {
        let records = vec![
            window(
                "a",
                "org.kde.kate.desktop",
                "kate",
                "kate",
                "notes",
                false,
                (800.0, 600.0),
            ),
            window(
                "b",
                "org.kde.katepart",
                "katepart",
                "katepart",
                "kate",
                false,
                (900.0, 700.0),
            ),
        ];
        assert_eq!(resolve_records(&records, "org.kde.kate").unwrap().uuid, "a");
    }

    #[test]
    fn resolution_never_guesses_between_equal_matches() {
        let records = vec![
            window("a", "kate", "kate", "kate", "kate", true, (1200.0, 800.0)),
            window("b", "kate", "kate", "kate", "kate", false, (800.0, 600.0)),
            window("c", "kate", "kate", "kate", "kate", false, (1000.0, 700.0)),
        ];
        let error = resolve_records(&records, "kate").unwrap_err().to_string();
        assert!(error.contains("kate@a"));
        assert!(error.contains("kate@b"));
        assert!(error.contains("kate@c"));
    }

    #[test]
    fn resolution_rejects_unsafe_ambiguity_and_empty_names() {
        let records = vec![
            window("a", "kate", "kate", "kate", "one", false, (800.0, 600.0)),
            window("b", "kate", "kate", "kate", "two", false, (1000.0, 700.0)),
        ];
        assert!(resolve_records(&records, "kate")
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        assert!(resolve_records(&records, "").is_err());
        assert!(resolve_records(&records, "Unnamed").is_err());
    }

    #[test]
    fn exact_window_ids_detect_stale_and_mismatched_records() {
        let records = vec![window(
            "{abc}",
            "org.kde.kate.desktop",
            "kate",
            "kate",
            "notes",
            false,
            (800.0, 600.0),
        )];
        let exact = exact_window_id(&records[0].info);
        assert_eq!(resolve_records(&records, &exact).unwrap().uuid, "{abc}");
        assert!(resolve_records(&records, "wrong@{abc}")
            .unwrap_err()
            .to_string()
            .contains("mismatch"));
        assert!(resolve_records(&records, "org.kde.kate@{stale}")
            .unwrap_err()
            .to_string()
            .contains("stale"));
    }

    #[test]
    fn converts_premultiplied_bgra_with_stride() {
        let raw = [
            25, 50, 100, 128, 0xaa, 0xbb, 0xcc, 0xdd, 0, 0, 0, 0, 0xee, 0xff, 0x11, 0x22,
        ];
        let rgba = bgra_to_rgba(&raw, 1, 2, 8, 6).unwrap();
        assert_eq!(rgba, [199, 100, 50, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn detects_nearly_black_frames() {
        let black = [0_u8, 0, 0, 255].repeat(1_000);
        assert!(frame_is_blank(&black));
        let mut visible = black;
        for pixel in visible.chunks_exact_mut(4).take(6) {
            pixel[..3].fill(255);
        }
        assert!(!frame_is_blank(&visible));
    }

    #[tokio::test]
    async fn async_pipe_reader_consumes_bytes_and_eof() {
        let (read_fd, write_fd) = create_pipe().unwrap();
        let payload = b"raw pixels";
        // SAFETY: `write_fd` is valid and payload points to initialized bytes.
        let written =
            unsafe { libc::write(write_fd.as_raw_fd(), payload.as_ptr().cast(), payload.len()) };
        assert_eq!(written, payload.len() as isize);
        drop(write_fd);
        assert_eq!(read_pipe(read_fd).await.unwrap(), payload);
    }

    #[test]
    fn parses_complete_window_metadata() {
        let metadata = HashMap::from([
            (
                "uuid".to_owned(),
                OwnedValue::from(zbus::zvariant::Str::from("abc")),
            ),
            (
                "caption".to_owned(),
                OwnedValue::from(zbus::zvariant::Str::from("Editor")),
            ),
            (
                "resourceClass".to_owned(),
                OwnedValue::from(zbus::zvariant::Str::from("kate")),
            ),
            (
                "resourceName".to_owned(),
                OwnedValue::from(zbus::zvariant::Str::from("kate")),
            ),
            (
                "desktopFile".to_owned(),
                OwnedValue::from(zbus::zvariant::Str::from("org.kde.kate")),
            ),
            ("pid".to_owned(), OwnedValue::from(123_i32)),
            ("x".to_owned(), OwnedValue::from(1.0_f64)),
            ("y".to_owned(), OwnedValue::from(2.0_f64)),
            ("width".to_owned(), OwnedValue::from(800.0_f64)),
            ("height".to_owned(), OwnedValue::from(600.0_f64)),
            ("minimized".to_owned(), OwnedValue::from(false)),
            ("fullscreen".to_owned(), OwnedValue::from(false)),
            ("type".to_owned(), OwnedValue::from(0_i32)),
            ("skipTaskbar".to_owned(), OwnedValue::from(false)),
        ]);
        let parsed = parse_window_metadata(&metadata).unwrap();
        assert_eq!(parsed.info.uuid, "abc");
        assert_eq!(parsed.info.pid, 123);
        assert_eq!(parsed.info.width, 800.0);
    }
}
