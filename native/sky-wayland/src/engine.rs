//! PID-correlated AT-SPI semantics bound to exact KWin windows.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use atspi::events::{ObjectEvents, WindowEvents};
use atspi::proxy::accessible::ObjectRefExt;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::text::TextProxy;
use atspi::proxy::value::ValueProxy;
use atspi::{AccessibilityConnection, CoordType, Interface, ObjectRefOwned, State};
use serde::Serialize;
use tokio::time::sleep;
use zbus::fdo::DBusProxy;

use crate::apps;
use crate::kwin::{exact_window_id, is_window_not_found, KWinBackend, WindowInfo};

const MAX_NODES: usize = 1_500;
const MAX_DEPTH: usize = 40;
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);
const LAUNCH_POLL: Duration = Duration::from_millis(250);
const DEFAULT_SETTLE_MS: u64 = 200;
const MAX_SETTLE_MS: u64 = 2_000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub id: String,
    pub display_name: Option<String>,
    pub is_running: bool,
}

pub struct AppState {
    pub window: WindowInfo,
    pub tree: String,
    pub screenshot_png: Vec<u8>,
    pub screenshot_width: u32,
    pub screenshot_height: u32,
    pub screenshot_scale: f64,
    pub focused_element: Option<FocusedElement>,
    pub accessibility_warning: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusedElement {
    pub index: i64,
    pub role: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct ElementTarget {
    pub center_x: f64,
    pub center_y: f64,
    pub click_action: Option<String>,
    pub window_uuid: String,
}

#[derive(Clone, Debug)]
pub struct PreparedPoint {
    pub x: f64,
    pub y: f64,
    pub window_uuid: String,
}

#[derive(Clone, Debug)]
struct NodeLine {
    index: i64,
    depth: usize,
    role: String,
    name: String,
    value: Option<String>,
    focused: bool,
    editable: bool,
    selected: bool,
    actions: Vec<String>,
    bounds: Option<(i32, i32, i32, i32)>,
}

struct Session {
    identity: String,
    window: WindowInfo,
    transform: CaptureTransform,
    index_map: HashMap<i64, ObjectRefOwned>,
}

#[derive(Clone)]
struct CaptureTransform {
    window_x: f64,
    window_y: f64,
    screenshot_width: u32,
    screenshot_height: u32,
    scale: f64,
}

struct AccessibilityRoot {
    name: String,
    pid: u32,
    object: ObjectRefOwned,
}

pub struct Engine {
    connection: AccessibilityConnection,
    kwin: KWinBackend,
    sessions: HashMap<String, Session>,
}

impl Engine {
    pub async fn new() -> Result<Self> {
        apps::enable_accessibility_status().await?;
        let connection = AccessibilityConnection::new().await.context(
            "AT-SPI accessibility bus is unavailable; KDE accessibility must be enabled",
        )?;
        connection
            .register_event::<ObjectEvents>()
            .await
            .context("failed to register AT-SPI object events")?;
        connection
            .register_event::<WindowEvents>()
            .await
            .context("failed to register AT-SPI window events")?;
        let kwin = KWinBackend::new()
            .await
            .context("KDE Plasma 6 with the KWin window and ScreenShot2 APIs is required")?;
        Ok(Self {
            connection,
            kwin,
            sessions: HashMap::new(),
        })
    }

    pub async fn list_apps(&self) -> Result<Vec<AppInfo>> {
        let mut result = Vec::new();
        for window in self.kwin.windows().await? {
            let identity = window_identity(&window);
            if identity.is_empty() || identity.eq_ignore_ascii_case("unnamed") {
                continue;
            }
            let display = if window.caption.trim().is_empty() {
                identity
            } else {
                window.caption.clone()
            };
            result.push(AppInfo {
                id: exact_window_id(&window),
                display_name: Some(display),
                is_running: true,
            });
        }
        result.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(result)
    }

    pub async fn snapshot(&mut self, app: &str, settle_ms: Option<u64>) -> Result<AppState> {
        let window = self.resolve_and_activate(app).await?;
        let settle_ms = settle_ms.unwrap_or(DEFAULT_SETTLE_MS);
        if settle_ms > MAX_SETTLE_MS {
            bail!("settle_ms must be at most {MAX_SETTLE_MS}");
        }
        sleep(Duration::from_millis(settle_ms)).await;
        let screenshot = self
            .kwin
            .screenshot(&window)
            .await
            .context("exact KWin window screenshot failed")?;
        let transform = CaptureTransform::new(
            &window,
            screenshot.width,
            screenshot.height,
            screenshot.scale,
        )?;
        let root = self.root_for_window(&window).await?;
        let (mut tree, index_map, focused_element, actionable) =
            self.build_tree(root.object, &transform).await?;
        let accessibility_warning = (!actionable).then(|| {
            format!(
                "PID-correlated AT-SPI tree for {app:?} has no bounded or interactive controls; fully quit and relaunch this Electron app with accessibility enabled (ACCESSIBILITY_ENABLED=1). Screenshot-relative coordinate actions remain available."
            )
        });
        if let Some(focused) = &focused_element {
            tree = format!(
                "focus: [{}] {} \"{}\"\n{tree}",
                focused.index,
                focused.role,
                escape(&focused.name)
            );
        } else {
            tree = format!("focus: none reported\n{tree}");
        }
        if let Some(warning) = &accessibility_warning {
            tree = format!("accessibility-warning: {warning}\n{tree}");
        }
        let key = normalize_app(app)?;
        self.sessions.insert(
            key,
            Session {
                identity: window_identity(&window),
                window: window.clone(),
                transform,
                index_map,
            },
        );
        Ok(AppState {
            window,
            tree,
            screenshot_png: screenshot.png,
            screenshot_width: screenshot.width,
            screenshot_height: screenshot.height,
            screenshot_scale: screenshot.scale,
            focused_element,
            accessibility_warning,
        })
    }

    pub async fn prepare_action(&self, app: &str) -> Result<WindowInfo> {
        self.resolve_and_activate(app).await
    }

    pub async fn translate_screenshot_point(
        &self,
        app: &str,
        x: f64,
        y: f64,
    ) -> Result<PreparedPoint> {
        validate_finite_point(x, y)?;
        let window = self.resolve_and_activate(app).await?;
        let session = self.valid_session(app, &window)?;
        if x < 0.0
            || x >= f64::from(session.transform.screenshot_width)
            || y < 0.0
            || y >= f64::from(session.transform.screenshot_height)
        {
            bail!(
                "screenshot-relative point ({x},{y}) is outside 0<=x<{}, 0<=y<{}",
                session.transform.screenshot_width,
                session.transform.screenshot_height
            );
        }
        let (x, y) = session.transform.screenshot_to_screen(x, y)?;
        Ok(PreparedPoint {
            x,
            y,
            window_uuid: window.uuid,
        })
    }

    pub async fn screenshot_center(&self, app: &str) -> Result<PreparedPoint> {
        let window = self.resolve_and_activate(app).await?;
        let session = self.valid_session(app, &window)?;
        let (x, y) = session.transform.screenshot_to_screen(
            f64::from(session.transform.screenshot_width) / 2.0,
            f64::from(session.transform.screenshot_height) / 2.0,
        )?;
        Ok(PreparedPoint {
            x,
            y,
            window_uuid: window.uuid,
        })
    }

    pub async fn element_target(&self, app: &str, element_index: i64) -> Result<ElementTarget> {
        let window = self.resolve_and_activate(app).await?;
        let object = self.indexed_object(app, &window, element_index)?;
        let proxy = object
            .as_accessible_proxy(self.connection.connection())
            .await
            .context("indexed accessibility element is no longer available")?;
        let interfaces = proxy.get_interfaces().await?;
        if !interfaces.contains(Interface::Component) {
            bail!("element {element_index} has no screen bounds");
        }
        let component = ComponentProxy::builder(self.connection.connection())
            .destination(
                object
                    .name()
                    .context("element has no AT-SPI bus name")?
                    .clone(),
            )?
            .path(object.path().clone())?
            .build()
            .await?;
        let (x, y, width, height) = component.get_extents(CoordType::Screen).await?;
        if width <= 0 || height <= 0 {
            bail!("element {element_index} has invalid screen bounds ({x},{y},{width},{height})");
        }
        let center_x = f64::from(x) + f64::from(width) / 2.0;
        let center_y = f64::from(y) + f64::from(height) / 2.0;
        ensure_screen_point_in_window(&window, center_x, center_y)?;
        let transform = &self.valid_session(app, &window)?.transform;
        let screenshot_point = transform.screen_to_screenshot(center_x, center_y)?;
        let (center_x, center_y) =
            transform.screenshot_to_screen(screenshot_point.0, screenshot_point.1)?;
        let click_action = self
            .action_names(object, interfaces)
            .await
            .into_iter()
            .find(|name| {
                matches!(
                    name.to_lowercase().as_str(),
                    "click" | "press" | "activate" | "jump" | "open" | "toggle"
                )
            });
        Ok(ElementTarget {
            center_x,
            center_y,
            click_action,
            window_uuid: window.uuid,
        })
    }

    pub async fn do_action(&self, app: &str, element_index: i64, action_name: &str) -> Result<()> {
        let window = self.resolve_and_activate(app).await?;
        let object = self.indexed_object(app, &window, element_index)?;
        let action = self.action_proxy(object).await?;
        let actions = action
            .get_actions()
            .await
            .context("element does not expose AT-SPI actions")?;
        let (index, _) = actions
            .iter()
            .enumerate()
            .find(|(_, candidate)| candidate.name.eq_ignore_ascii_case(action_name))
            .with_context(|| {
                format!("action {action_name:?} is not exposed by element {element_index}")
            })?;
        if !action
            .do_action(i32::try_from(index).context("AT-SPI action index overflow")?)
            .await?
        {
            bail!("AT-SPI action {action_name:?} reported failure");
        }
        Ok(())
    }

    pub async fn set_value(&self, app: &str, element_index: i64, value: &str) -> Result<()> {
        let window = self.resolve_and_activate(app).await?;
        let object = self.indexed_object(app, &window, element_index)?;
        let accessible = object
            .as_accessible_proxy(self.connection.connection())
            .await
            .context("indexed accessibility element is no longer available")?;
        if !accessible
            .get_interfaces()
            .await?
            .contains(Interface::EditableText)
        {
            bail!("element {element_index} does not implement AT-SPI EditableText");
        }
        let editable = EditableTextProxy::builder(self.connection.connection())
            .destination(
                object
                    .name()
                    .context("element has no AT-SPI bus name")?
                    .clone(),
            )?
            .path(object.path().clone())?
            .build()
            .await?;
        if !editable.set_text_contents(value).await? {
            bail!("AT-SPI EditableText rejected the new value");
        }
        Ok(())
    }

    pub async fn select_text(
        &self,
        app: &str,
        element_index: i64,
        text: &str,
        prefix: Option<&str>,
        suffix: Option<&str>,
        selection_type: Option<&str>,
    ) -> Result<()> {
        let window = self.resolve_and_activate(app).await?;
        let object = self.indexed_object(app, &window, element_index)?;
        let accessible = object
            .as_accessible_proxy(self.connection.connection())
            .await
            .context("indexed accessibility element is no longer available")?;
        if !accessible.get_interfaces().await?.contains(Interface::Text) {
            bail!("element {element_index} does not implement AT-SPI Text");
        }
        let proxy = TextProxy::builder(self.connection.connection())
            .destination(
                object
                    .name()
                    .context("element has no AT-SPI bus name")?
                    .clone(),
            )?
            .path(object.path().clone())?
            .build()
            .await?;
        let full_text = proxy.get_text(0, proxy.character_count().await?).await?;
        let (start, end) = find_text_offsets(&full_text, text, prefix, suffix)
            .with_context(|| format!("text {text:?} not found in element {element_index}"))?;
        match selection_type.unwrap_or("text") {
            "text" => {
                let changed = if proxy.get_n_selections().await.unwrap_or(0) > 0 {
                    proxy.set_selection(0, start, end).await?
                } else {
                    proxy.add_selection(start, end).await?
                };
                if !changed {
                    bail!("AT-SPI Text rejected the requested selection");
                }
            }
            "cursor_before" => {
                if !proxy.set_caret_offset(start).await? {
                    bail!("AT-SPI Text rejected the requested caret position");
                }
            }
            "cursor_after" => {
                if !proxy.set_caret_offset(end).await? {
                    bail!("AT-SPI Text rejected the requested caret position");
                }
            }
            other => bail!(
                "invalid selection_type {other:?}; expected text, cursor_before, or cursor_after"
            ),
        }
        Ok(())
    }

    async fn resolve_and_activate(&self, app: &str) -> Result<WindowInfo> {
        normalize_app(app)?;
        let desktop_alias = match apps::resolve_desktop_app(app) {
            Ok(desktop) => Some(desktop),
            Err(error) if apps::is_desktop_app_not_found(&error) => None,
            Err(error) => return Err(error),
        };
        let lookup = desktop_alias
            .as_ref()
            .map(|desktop| desktop.id.clone())
            .unwrap_or_else(|| app.to_owned());
        let window = match self.kwin.resolve_window(&lookup).await {
            Ok(window) => window,
            Err(error) if is_window_not_found(&error) => {
                let desktop = match desktop_alias {
                    Some(desktop) => desktop,
                    None => apps::resolve_desktop_app(app)?,
                };
                apps::launch_app(&desktop).await?;
                let deadline = Instant::now() + LAUNCH_TIMEOUT;
                loop {
                    match self.kwin.resolve_window(&desktop.id).await {
                        Ok(window)
                            if canonical_identity(&window_identity(&window))
                                == canonical_identity(&desktop.id) =>
                        {
                            break window;
                        }
                        Ok(window) => bail!(
                            "launched desktop application {} but KWin returned mismatched identity {:?} for UUID {}",
                            desktop.id,
                            window_identity(&window),
                            window.uuid
                        ),
                        Err(error)
                            if is_window_not_found(&error) && Instant::now() < deadline =>
                        {
                            sleep(LAUNCH_POLL).await;
                        }
                        Err(error) if is_window_not_found(&error) => {
                            bail!(
                                "launched desktop application {} but no exact actionable KWin window appeared within {} seconds",
                                desktop.id,
                                LAUNCH_TIMEOUT.as_secs()
                            );
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            Err(error) => return Err(error),
        };
        let requested = exact_id_identity(&lookup)
            .map(canonical_identity)
            .unwrap_or_else(|| canonical_identity(&lookup));
        let resolved = canonical_identity(&window_identity(&window));
        if requested != resolved {
            bail!(
                "KWin resolved {app:?} to non-exact window identity {:?}; use an exact app ID from list_apps",
                window_identity(&window)
            );
        }
        self.kwin.activate(&window).await?;
        Ok(window)
    }

    async fn root_for_window(&self, window: &WindowInfo) -> Result<AccessibilityRoot> {
        let roots = self.accessibility_roots().await?;
        let mut matches = Vec::new();
        let mut evidence = Vec::new();
        for root in roots {
            let exact_pid = root.pid == window.pid;
            let descendant =
                !exact_pid && apps::process_is_same_or_descendant(window.pid, root.pid);
            let name_matches = root_name_matches_window(&root.name, window);
            let accepted =
                !root.name.trim().is_empty() && (exact_pid || (descendant && name_matches));
            if exact_pid || descendant || name_matches {
                evidence.push(format!(
                    "{:?} (PID {}, exact_pid={}, descendant={}, name_match={})",
                    root.name, root.pid, exact_pid, descendant, name_matches
                ));
            }
            if accepted {
                matches.push(root);
            }
        }
        let evidence = if evidence.is_empty() {
            "no related or identity-matching roots".to_string()
        } else {
            evidence.join("; ")
        };
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => bail!(
                "window {} (PID {}, identity {:?}) has no strictly correlated AT-SPI root ({evidence}); the already-running app must be fully restarted with accessibility enabled",
                window.uuid,
                window.pid,
                window_identity(window),
            ),
            _ => {
                let candidates = matches
                    .iter()
                    .map(|root| format!("{:?} (PID {})", root.name, root.pid))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "window {} (PID {}) has ambiguous strictly correlated AT-SPI roots: {candidates}; evidence: {evidence}",
                    window.uuid,
                    window.pid
                )
            }
        }
    }

    async fn accessibility_roots(&self) -> Result<Vec<AccessibilityRoot>> {
        let registry = self
            .connection
            .root_accessible_on_registry()
            .await
            .context("failed to access the AT-SPI registry root")?;
        let children = registry
            .get_children()
            .await
            .context("failed to enumerate AT-SPI application roots")?;
        let dbus = DBusProxy::new(self.connection.connection())
            .await
            .context("failed to access org.freedesktop.DBus on the accessibility bus")?;
        let mut roots = Vec::new();
        for object in children {
            if object.is_null() {
                continue;
            }
            let Some(name) = object.name().cloned() else {
                continue;
            };
            let pid = dbus
                .get_connection_unix_process_id(name.clone().into())
                .await
                .with_context(|| format!("failed to derive PID for AT-SPI bus name {name}"))?;
            let display_name = match object
                .as_accessible_proxy(self.connection.connection())
                .await
            {
                Ok(proxy) => proxy.name().await.unwrap_or_default(),
                Err(_) => String::new(),
            };
            roots.push(AccessibilityRoot {
                name: display_name,
                pid,
                object,
            });
        }
        Ok(roots)
    }

    async fn build_tree(
        &self,
        root: ObjectRefOwned,
        transform: &CaptureTransform,
    ) -> Result<(
        String,
        HashMap<i64, ObjectRefOwned>,
        Option<FocusedElement>,
        bool,
    )> {
        let mut stack = vec![(root, 0_usize)];
        let mut index_map = HashMap::new();
        let mut lines = Vec::new();
        let mut visited = 0_usize;
        let mut truncated = false;
        while let Some((object, depth)) = stack.pop() {
            if visited >= MAX_NODES {
                truncated = true;
                break;
            }
            visited += 1;
            if depth > MAX_DEPTH {
                truncated = true;
                continue;
            }
            let proxy = match object
                .as_accessible_proxy(self.connection.connection())
                .await
            {
                Ok(proxy) => proxy,
                Err(_) => continue,
            };
            for child in proxy
                .get_children()
                .await
                .unwrap_or_default()
                .into_iter()
                .rev()
            {
                if !child.is_null() {
                    stack.push((child, depth + 1));
                }
            }
            let name = proxy.name().await.unwrap_or_default();
            let description = proxy.description().await.unwrap_or_default();
            let display_name = if name.trim().is_empty() {
                description
            } else {
                name
            };
            let states = proxy.get_state().await.unwrap_or_default();
            if states.contains(State::Defunct)
                || (!states.contains(State::Showing) && display_name.trim().is_empty())
            {
                continue;
            }
            let interfaces = proxy.get_interfaces().await.unwrap_or_default();
            let role = proxy
                .get_role_name()
                .await
                .unwrap_or_else(|_| "unknown".to_string());
            let actions = self.action_names(&object, interfaces).await;
            let value = self.element_value(&object, interfaces).await;
            let bounds = self
                .element_bounds(&object, interfaces)
                .await
                .and_then(|bounds| transform.screen_rect_to_screenshot(bounds).ok());
            let index = i64::try_from(lines.len()).context("accessibility tree index overflow")?;
            index_map.insert(index, object);
            lines.push(NodeLine {
                index,
                depth,
                role,
                name: display_name,
                value,
                focused: states.contains(State::Focused),
                editable: interfaces.contains(Interface::EditableText),
                selected: states.contains(State::Selected),
                actions,
                bounds,
            });
        }
        let actionable = tree_is_actionable(&lines);
        let focused = lines
            .iter()
            .find(|line| line.focused)
            .map(|line| FocusedElement {
                index: line.index,
                role: line.role.clone(),
                name: line.name.clone(),
            });
        let mut output = lines
            .iter()
            .map(format_node_line)
            .collect::<Vec<_>>()
            .join("\n");
        if truncated {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("[accessibility tree truncated]");
        }
        Ok((output, index_map, focused, actionable))
    }

    fn valid_session<'a>(&'a self, app: &str, window: &WindowInfo) -> Result<&'a Session> {
        let key = normalize_app(app)?;
        let session = self.sessions.get(&key).with_context(|| {
            format!("no screenshot session for {app:?}; call get_app_state first")
        })?;
        if session.window.uuid != window.uuid {
            bail!(
                "stale app session: screenshot window UUID {} changed to {}; call get_app_state again",
                session.window.uuid,
                window.uuid
            );
        }
        if session.identity != window_identity(window) || !same_geometry(&session.window, window) {
            bail!("app window identity or geometry changed; call get_app_state again");
        }
        Ok(session)
    }

    fn indexed_object(
        &self,
        app: &str,
        window: &WindowInfo,
        element_index: i64,
    ) -> Result<&ObjectRefOwned> {
        self.valid_session(app, window)?
            .index_map
            .get(&element_index)
            .with_context(|| {
                format!(
                    "unknown element_index {element_index} for {app:?}; call get_app_state again"
                )
            })
    }

    async fn action_proxy<'a>(&'a self, object: &'a ObjectRefOwned) -> Result<ActionProxy<'a>> {
        Ok(ActionProxy::builder(self.connection.connection())
            .destination(
                object
                    .name()
                    .context("element has no AT-SPI bus name")?
                    .clone(),
            )?
            .path(object.path().clone())?
            .build()
            .await?)
    }

    async fn action_names(
        &self,
        object: &ObjectRefOwned,
        interfaces: atspi::InterfaceSet,
    ) -> Vec<String> {
        if !interfaces.contains(Interface::Action) {
            return Vec::new();
        }
        match self.action_proxy(object).await {
            Ok(proxy) => proxy
                .get_actions()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|action| action.name)
                .filter(|name| !name.trim().is_empty())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn element_value(
        &self,
        object: &ObjectRefOwned,
        interfaces: atspi::InterfaceSet,
    ) -> Option<String> {
        if interfaces.contains(Interface::Value) {
            let proxy = ValueProxy::builder(self.connection.connection())
                .destination(object.name()?.clone())
                .ok()?
                .path(object.path().clone())
                .ok()?
                .build()
                .await
                .ok()?;
            if let Ok(text) = proxy.text().await {
                if !text.is_empty() {
                    return Some(truncate(&text, 200));
                }
            }
            return proxy
                .current_value()
                .await
                .ok()
                .map(|value| value.to_string());
        }
        if interfaces.contains(Interface::Text) {
            let proxy = TextProxy::builder(self.connection.connection())
                .destination(object.name()?.clone())
                .ok()?
                .path(object.path().clone())
                .ok()?
                .build()
                .await
                .ok()?;
            let count = proxy.character_count().await.ok()?.min(201);
            let text = proxy.get_text(0, count).await.ok()?;
            if !text.is_empty() {
                return Some(truncate(&text, 200));
            }
        }
        None
    }

    async fn element_bounds(
        &self,
        object: &ObjectRefOwned,
        interfaces: atspi::InterfaceSet,
    ) -> Option<(i32, i32, i32, i32)> {
        if !interfaces.contains(Interface::Component) {
            return None;
        }
        let proxy = ComponentProxy::builder(self.connection.connection())
            .destination(object.name()?.clone())
            .ok()?
            .path(object.path().clone())
            .ok()?
            .build()
            .await
            .ok()?;
        let bounds = proxy.get_extents(CoordType::Screen).await.ok()?;
        (bounds.2 > 0 && bounds.3 > 0).then_some(bounds)
    }
}

impl CaptureTransform {
    fn new(
        window: &WindowInfo,
        screenshot_width: u32,
        screenshot_height: u32,
        scale: f64,
    ) -> Result<Self> {
        if screenshot_width == 0 || screenshot_height == 0 || !scale.is_finite() || scale <= 0.0 {
            bail!("KWin screenshot has invalid dimensions or scale");
        }
        let expected_width = window.width * scale;
        let expected_height = window.height * scale;
        if (expected_width - f64::from(screenshot_width)).abs() > 2.0
            || (expected_height - f64::from(screenshot_height)).abs() > 2.0
        {
            bail!(
                "KWin screenshot {}x{} at scale {} does not match window frame {}x{}; refusing ambiguous coordinates",
                screenshot_width,
                screenshot_height,
                scale,
                window.width,
                window.height
            );
        }
        Ok(Self {
            window_x: window.x,
            window_y: window.y,
            screenshot_width,
            screenshot_height,
            scale,
        })
    }

    fn screenshot_to_screen(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        validate_finite_point(x, y)?;
        if x < 0.0
            || x >= f64::from(self.screenshot_width)
            || y < 0.0
            || y >= f64::from(self.screenshot_height)
        {
            bail!(
                "screenshot-relative point ({x},{y}) is outside 0<=x<{}, 0<=y<{}",
                self.screenshot_width,
                self.screenshot_height
            );
        }
        Ok((
            self.window_x + x / self.scale,
            self.window_y + y / self.scale,
        ))
    }

    fn screen_to_screenshot(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let screenshot_x = (x - self.window_x) * self.scale;
        let screenshot_y = (y - self.window_y) * self.scale;
        validate_finite_point(screenshot_x, screenshot_y)?;
        if screenshot_x < 0.0
            || screenshot_x >= f64::from(self.screenshot_width)
            || screenshot_y < 0.0
            || screenshot_y >= f64::from(self.screenshot_height)
        {
            bail!("screen point ({x},{y}) lies outside the captured window");
        }
        Ok((screenshot_x, screenshot_y))
    }

    fn screen_rect_to_screenshot(
        &self,
        bounds: (i32, i32, i32, i32),
    ) -> Result<(i32, i32, i32, i32)> {
        let (x, y, width, height) = bounds;
        let screenshot_x = (f64::from(x) - self.window_x) * self.scale;
        let screenshot_y = (f64::from(y) - self.window_y) * self.scale;
        let screenshot_width = f64::from(width) * self.scale;
        let screenshot_height = f64::from(height) * self.scale;
        [
            screenshot_x,
            screenshot_y,
            screenshot_width,
            screenshot_height,
        ]
        .into_iter()
        .all(f64::is_finite)
        .then_some(())
        .context("AT-SPI bounds cannot be represented in screenshot coordinates")?;
        Ok((
            screenshot_x.round() as i32,
            screenshot_y.round() as i32,
            screenshot_width.round() as i32,
            screenshot_height.round() as i32,
        ))
    }
}

fn normalize_app(app: &str) -> Result<String> {
    let normalized = app.trim().to_lowercase();
    if normalized.is_empty() || normalized == "unnamed" {
        bail!("app must be non-empty and cannot be \"Unnamed\"");
    }
    Ok(normalized)
}

fn window_identity(window: &WindowInfo) -> String {
    let desktop = window.desktop_file.trim();
    if !desktop.is_empty() {
        desktop.to_string()
    } else {
        window.resource_class.trim().to_string()
    }
}

fn canonical_identity(value: &str) -> String {
    let normalized = value.trim().to_lowercase();
    normalized
        .strip_suffix(".desktop")
        .unwrap_or(&normalized)
        .to_string()
}

fn exact_id_identity(value: &str) -> Option<&str> {
    let (identity, uuid) = value.rsplit_once('@')?;
    (uuid.starts_with('{') && uuid.ends_with('}')).then_some(identity)
}

fn root_name_matches_window(root_name: &str, window: &WindowInfo) -> bool {
    let root = canonical_identity(root_name);
    !root.is_empty()
        && [
            window_identity(window),
            window.resource_class.clone(),
            window.resource_name.clone(),
            window.caption.clone(),
        ]
        .iter()
        .any(|candidate| !candidate.trim().is_empty() && canonical_identity(candidate) == root)
}

fn same_geometry(left: &WindowInfo, right: &WindowInfo) -> bool {
    [
        left.x - right.x,
        left.y - right.y,
        left.width - right.width,
        left.height - right.height,
    ]
    .into_iter()
    .all(|difference| difference.abs() < 0.5)
}

fn validate_finite_point(x: f64, y: f64) -> Result<()> {
    if !x.is_finite() || !y.is_finite() {
        bail!("screenshot-relative coordinates must be finite");
    }
    Ok(())
}

fn ensure_screen_point_in_window(window: &WindowInfo, x: f64, y: f64) -> Result<()> {
    if !x.is_finite()
        || !y.is_finite()
        || x < window.x
        || x >= window.x + window.width
        || y < window.y
        || y >= window.y + window.height
    {
        bail!(
            "element center ({x},{y}) lies outside exact window {} frame ({},{},{},{})",
            window.uuid,
            window.x,
            window.y,
            window.width,
            window.height
        );
    }
    Ok(())
}

fn tree_is_actionable(lines: &[NodeLine]) -> bool {
    lines.iter().any(|line| {
        !line.actions.is_empty()
            || line.editable
            || line
                .value
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
            || (line.depth >= 2 && line.bounds.is_some())
    })
}

fn format_node_line(node: &NodeLine) -> String {
    let mut line = format!(
        "{}[{}] {} \"{}\"",
        "  ".repeat(node.depth),
        node.index,
        node.role,
        escape(&node.name)
    );
    if let Some(value) = &node.value {
        line.push_str(&format!(" value:\"{}\"", escape(value)));
    }
    if node.focused {
        line.push_str(" *focused*");
    }
    if node.editable {
        line.push_str(" [editable]");
    }
    if node.selected {
        line.push_str(" *selected*");
    }
    if !node.actions.is_empty() {
        line.push_str(&format!(" actions:[{}]", node.actions.join(",")));
    }
    if let Some((x, y, width, height)) = node.bounds {
        line.push_str(&format!(" @({x},{y},{width}×{height})"));
    }
    line
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn find_text_offsets(
    haystack: &str,
    needle: &str,
    prefix: Option<&str>,
    suffix: Option<&str>,
) -> Option<(i32, i32)> {
    if needle.is_empty() {
        return None;
    }
    for (byte_start, matched) in haystack.match_indices(needle) {
        let byte_end = byte_start + matched.len();
        if prefix.is_some_and(|value| !haystack[..byte_start].ends_with(value))
            || suffix.is_some_and(|value| !haystack[byte_end..].starts_with(value))
        {
            continue;
        }
        let start = haystack[..byte_start].chars().count();
        let end = start + needle.chars().count();
        return Some((i32::try_from(start).ok()?, i32::try_from(end).ok()?));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(uuid: &str, x: f64, y: f64, width: f64, height: f64) -> WindowInfo {
        WindowInfo {
            uuid: uuid.to_string(),
            caption: "test".to_string(),
            resource_class: "test".to_string(),
            resource_name: "test".to_string(),
            desktop_file: "test.desktop".to_string(),
            pid: std::process::id(),
            x,
            y,
            width,
            height,
            minimized: false,
            fullscreen: false,
        }
    }

    #[test]
    fn rejects_empty_app_arguments() {
        assert!(normalize_app("").is_err());
        assert!(normalize_app("Unnamed").is_err());
        assert_eq!(canonical_identity("Steam.desktop"), "steam");
        assert_ne!(canonical_identity("ste"), canonical_identity("steam"));
    }

    #[test]
    fn descendant_roots_require_exact_window_identity_names() {
        let mut target = window("{a}", 0.0, 0.0, 10.0, 10.0);
        target.caption = "Visual Studio Code".to_owned();
        target.resource_class = "code".to_owned();
        target.resource_name = "code".to_owned();
        target.desktop_file = "code.desktop".to_owned();
        assert!(root_name_matches_window("Code", &target));
        assert!(root_name_matches_window("code.desktop", &target));
        assert!(root_name_matches_window("Visual Studio Code", &target));
        assert!(!root_name_matches_window("terminal-code-launcher", &target));
        assert!(!root_name_matches_window("", &target));
    }

    #[test]
    fn capture_transform_round_trips_scaled_pixels() {
        let window = window("a", 10.0, 20.0, 100.0, 50.0);
        let transform = CaptureTransform::new(&window, 200, 100, 2.0).unwrap();
        assert_eq!(
            transform.screenshot_to_screen(100.0, 50.0).unwrap(),
            (60.0, 45.0)
        );
        assert_eq!(
            transform.screen_to_screenshot(60.0, 45.0).unwrap(),
            (100.0, 50.0)
        );
    }

    #[test]
    fn stale_uuid_and_geometry_are_distinct() {
        assert_ne!(
            window("a", 0.0, 0.0, 10.0, 10.0).uuid,
            window("b", 0.0, 0.0, 10.0, 10.0).uuid
        );
        assert!(!same_geometry(
            &window("a", 0.0, 0.0, 10.0, 10.0),
            &window("a", 1.0, 0.0, 10.0, 10.0)
        ));
    }

    #[test]
    fn shallow_application_tree_is_not_actionable() {
        let root = NodeLine {
            index: 0,
            depth: 0,
            role: "application".to_string(),
            name: "App".to_string(),
            value: None,
            focused: false,
            editable: false,
            selected: false,
            actions: vec![],
            bounds: None,
        };
        let window = NodeLine {
            index: 1,
            depth: 1,
            role: "frame".to_string(),
            name: "App".to_string(),
            value: None,
            focused: true,
            editable: false,
            selected: false,
            actions: vec![],
            bounds: Some((0, 0, 800, 600)),
        };
        assert!(!tree_is_actionable(&[root, window]));
    }

    #[test]
    fn rejects_non_finite_coordinates() {
        assert!(validate_finite_point(f64::NAN, 0.0).is_err());
        assert!(validate_finite_point(0.0, f64::INFINITY).is_err());
    }

    #[test]
    fn finds_unicode_character_offsets() {
        assert_eq!(
            find_text_offsets("α hello β hello γ", "hello", Some("β "), Some(" γ")),
            Some((10, 15))
        );
    }
}
