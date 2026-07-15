//! Exact-window semantic Computer Use MCP server for KDE Plasma.

#[path = "../apps.rs"]
#[allow(dead_code)]
mod apps;
#[path = "../engine.rs"]
mod engine;
#[path = "../kwin.rs"]
mod kwin;

use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use engine::Engine;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

const SKY_TIMEOUT_MS: u64 = 10_000;

fn sky_bin() -> PathBuf {
    std::env::var("SKY_WAYLAND_BIN")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("sky_wayland"))
                .with_file_name("sky_wayland")
        })
}

fn run_sky(command: &str, input: &Value) -> Result<String> {
    let mut child = Command::new(sky_bin())
        .arg("--timeout-ms")
        .arg(SKY_TIMEOUT_MS.to_string())
        .arg("--mouse-size-px")
        .arg("12")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to launch sky_wayland {command}"))?;
    child
        .stdin
        .take()
        .context("sky_wayland stdin")?
        .write_all(serde_json::to_string(input)?.as_bytes())?;
    let deadline = Instant::now() + Duration::from_millis(SKY_TIMEOUT_MS + 1_000);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("sky_wayland {command} exceeded its hard timeout");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .context("sky_wayland stdout")?
        .read_to_end(&mut stdout)?;
    child
        .stderr
        .take()
        .context("sky_wayland stderr")?
        .read_to_end(&mut stderr)?;
    if !status.success() {
        bail!("{}", String::from_utf8_lossy(&stderr).trim());
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "list_apps",
            "description": "List exact actionable KWin app IDs. Pass these IDs as app.",
            "inputSchema": {"type":"object","properties":{},"additionalProperties":false}
        },
        {
            "name": "get_app_state",
            "description": "Activate one exact app window, wait briefly for rendering, and return its PID-correlated semantic tree plus exact-window PNG. Required before coordinate or element-index actions.",
            "inputSchema": {
                "type":"object",
                "properties":{
                    "app":{"type":"string","description":"Exact UUID-qualified app ID from list_apps"},
                    "settle_ms":{"type":"integer","minimum":0,"maximum":2000,"description":"Render settle delay before capture; defaults to 200 ms"}
                },
                "required":["app"],"additionalProperties":false
            }
        },
        {
            "name":"click",
            "description":"Click an indexed element or screenshot-relative x/y in the exact app window. Coordinates require get_app_state.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "app":{"type":"string","description":"Exact app ID from list_apps"},
                    "element_index":{"type":"integer"},
                    "x":{"type":"number","description":"X in latest app screenshot pixels"},
                    "y":{"type":"number","description":"Y in latest app screenshot pixels"},
                    "mouse_button":{"type":"string","enum":["left","right","middle","l","r","m"]},
                    "click_count":{"type":"integer","minimum":1,"maximum":3}
                },
                "required":["app"],"additionalProperties":false
            }
        },
        {
            "name":"drag",
            "description":"Drag between two screenshot-relative points in the exact app window. Requires get_app_state.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "app":{"type":"string","description":"Exact app ID from list_apps"},
                    "from_x":{"type":"number"},"from_y":{"type":"number"},
                    "to_x":{"type":"number"},"to_y":{"type":"number"}
                },
                "required":["app","from_x","from_y","to_x","to_y"],"additionalProperties":false
            }
        },
        {
            "name":"scroll",
            "description":"Scroll at an indexed element, screenshot-relative x/y, or latest screenshot center in the exact app. Requires get_app_state.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "app":{"type":"string","description":"Exact app ID from list_apps"},
                    "element_index":{"type":"integer"},
                    "direction":{"type":"string","enum":["up","down","left","right","u","d","l","r"]},
                    "pages":{"type":"number","exclusiveMinimum":0,"maximum":20},
                    "x":{"type":"number","description":"X in latest app screenshot pixels"},
                    "y":{"type":"number","description":"Y in latest app screenshot pixels"}
                },
                "required":["app","direction"],"additionalProperties":false
            }
        },
        {
            "name":"press_key",
            "description":"Activate the exact app ID from list_apps, verify its KWin UUID, then press a key or chord.",
            "inputSchema":{"type":"object","properties":{"app":{"type":"string"},"key":{"type":"string"}},"required":["app","key"],"additionalProperties":false}
        },
        {
            "name":"type_text",
            "description":"Activate the exact app ID from list_apps, verify its KWin UUID, then type text.",
            "inputSchema":{"type":"object","properties":{"app":{"type":"string"},"text":{"type":"string"}},"required":["app","text"],"additionalProperties":false}
        },
        {
            "name":"set_value",
            "description":"Activate the exact app and set a current-session EditableText element. Requires get_app_state.",
            "inputSchema":{"type":"object","properties":{"app":{"type":"string"},"element_index":{"type":"integer"},"value":{"type":"string"}},"required":["app","element_index","value"],"additionalProperties":false}
        },
        {
            "name":"select_text",
            "description":"Activate the exact app and select text in a current-session element. Requires get_app_state.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "app":{"type":"string"},"element_index":{"type":"integer"},"text":{"type":"string"},
                    "prefix":{"type":"string"},"suffix":{"type":"string"},
                    "selection_type":{"type":"string","enum":["text","cursor_before","cursor_after"]}
                },
                "required":["app","element_index","text"],"additionalProperties":false
            }
        },
        {
            "name":"perform_secondary_action",
            "description":"Activate the exact app and invoke a named AT-SPI action on a current-session element. Requires get_app_state.",
            "inputSchema":{"type":"object","properties":{"app":{"type":"string"},"element_index":{"type":"integer"},"action":{"type":"string"}},"required":["app","element_index","action"],"additionalProperties":false}
        }
    ])
}

async fn call_tool(engine: &Mutex<Option<Engine>>, name: &str, arguments: &Value) -> Result<Value> {
    let empty = json!({});
    let args = if arguments.is_null() {
        &empty
    } else {
        arguments
    };
    match name {
        "list_apps" => {
            let mut guard = engine.lock().await;
            let apps = get_engine(&mut guard).await?.list_apps().await?;
            text_result(serde_json::to_string(&apps)?)
        }
        "get_app_state" => {
            let app = string_arg(args, "app")?;
            let settle_ms = optional_u64(args, "settle_ms")?;
            let state = {
                let mut guard = engine.lock().await;
                get_engine(&mut guard)
                    .await?
                    .snapshot(app, settle_ms)
                    .await?
            };
            let encoded = base64::engine::general_purpose::STANDARD.encode(state.screenshot_png);
            Ok(json!({
                "content":[
                    {"type":"image","data":encoded,"mimeType":"image/png"},
                    {"type":"text","text":state.tree}
                ],
                "window": state.window,
                "screenshotWidth": state.screenshot_width,
                "screenshotHeight": state.screenshot_height,
                "screenshotScale": state.screenshot_scale,
                "coordinateSpace": "screenshot",
                "focusedElement": state.focused_element,
                "accessibilityWarning": state.accessibility_warning
            }))
        }
        "click" => {
            let app = string_arg(args, "app")?;
            let button = normalize_mouse_button(optional_string(args, "mouse_button")?)?;
            let count = optional_i64(args, "click_count")?.unwrap_or(1);
            if !(1..=3).contains(&count) {
                bail!("click_count must be between 1 and 3");
            }
            let (x, y, expected_window_uuid) =
                if let Some(index) = optional_i64(args, "element_index")? {
                    reject_coordinates_with_element(args)?;
                    let target = {
                        let mut guard = engine.lock().await;
                        get_engine(&mut guard)
                            .await?
                            .element_target(app, index)
                            .await?
                    };
                    if is_left_button(button) && count == 1 {
                        if let Some(action) = target.click_action {
                            let mut guard = engine.lock().await;
                            get_engine(&mut guard)
                                .await?
                                .do_action(app, index, &action)
                                .await?;
                            return ok_result();
                        }
                    }
                    (target.center_x, target.center_y, target.window_uuid)
                } else {
                    let x = number_arg(args, "x")?;
                    let y = number_arg(args, "y")?;
                    let mut guard = engine.lock().await;
                    let point = get_engine(&mut guard)
                        .await?
                        .translate_screenshot_point(app, x, y)
                        .await?;
                    (point.x, point.y, point.window_uuid)
                };
            let mut payload = Map::from_iter([
                ("x".to_string(), json!(x)),
                ("y".to_string(), json!(y)),
                ("click_count".to_string(), json!(count)),
                (
                    "expected_window_uuid".to_string(),
                    json!(expected_window_uuid),
                ),
            ]);
            insert_optional(&mut payload, "mouse_button", button.map(Value::from));
            run_sky("click", &Value::Object(payload))?;
            ok_result()
        }
        "drag" => {
            let app = string_arg(args, "app")?;
            let (from, to) = {
                let mut guard = engine.lock().await;
                let engine = get_engine(&mut guard).await?;
                let from = engine
                    .translate_screenshot_point(
                        app,
                        number_arg(args, "from_x")?,
                        number_arg(args, "from_y")?,
                    )
                    .await?;
                let to = engine
                    .translate_screenshot_point(
                        app,
                        number_arg(args, "to_x")?,
                        number_arg(args, "to_y")?,
                    )
                    .await?;
                (from, to)
            };
            if from.window_uuid != to.window_uuid {
                bail!("app window changed while preparing drag; call get_app_state again");
            }
            run_sky(
                "drag",
                &json!({
                    "from_x":from.x,"from_y":from.y,
                    "to_x":to.x,"to_y":to.y,
                    "expected_window_uuid":from.window_uuid
                }),
            )?;
            ok_result()
        }
        "scroll" => {
            let app = string_arg(args, "app")?;
            let direction = normalize_direction(string_arg(args, "direction")?)?;
            let pages = validate_pages(optional_number(args, "pages")?.unwrap_or(1.0))?;
            let point = if let Some(index) = optional_i64(args, "element_index")? {
                reject_coordinates_with_element(args)?;
                let mut guard = engine.lock().await;
                let target = get_engine(&mut guard)
                    .await?
                    .element_target(app, index)
                    .await?;
                engine::PreparedPoint {
                    x: target.center_x,
                    y: target.center_y,
                    window_uuid: target.window_uuid,
                }
            } else if let Some((x, y)) = optional_coordinate_pair(args)? {
                let mut guard = engine.lock().await;
                get_engine(&mut guard)
                    .await?
                    .translate_screenshot_point(app, x, y)
                    .await?
            } else {
                let mut guard = engine.lock().await;
                get_engine(&mut guard).await?.screenshot_center(app).await?
            };
            run_sky(
                "scroll",
                &json!({
                    "direction":direction,
                    "pixels":(pages * 600.0).round(),
                    "x":point.x,
                    "y":point.y,
                    "expected_window_uuid":point.window_uuid
                }),
            )?;
            ok_result()
        }
        "press_key" => {
            let app = string_arg(args, "app")?;
            let mut guard = engine.lock().await;
            let window = get_engine(&mut guard).await?.prepare_action(app).await?;
            run_sky(
                "press_key",
                &json!({
                    "key":string_arg(args, "key")?,
                    "expected_window_uuid":window.uuid
                }),
            )?;
            ok_result()
        }
        "type_text" => {
            let app = string_arg(args, "app")?;
            let mut guard = engine.lock().await;
            let window = get_engine(&mut guard).await?.prepare_action(app).await?;
            run_sky(
                "type_text",
                &json!({
                    "text":string_arg(args, "text")?,
                    "expected_window_uuid":window.uuid
                }),
            )?;
            ok_result()
        }
        "set_value" => {
            let mut guard = engine.lock().await;
            get_engine(&mut guard)
                .await?
                .set_value(
                    string_arg(args, "app")?,
                    i64_arg(args, "element_index")?,
                    string_arg(args, "value")?,
                )
                .await?;
            ok_result()
        }
        "select_text" => {
            let mut guard = engine.lock().await;
            get_engine(&mut guard)
                .await?
                .select_text(
                    string_arg(args, "app")?,
                    i64_arg(args, "element_index")?,
                    string_arg(args, "text")?,
                    optional_string(args, "prefix")?,
                    optional_string(args, "suffix")?,
                    optional_string(args, "selection_type")?,
                )
                .await?;
            ok_result()
        }
        "perform_secondary_action" => {
            let mut guard = engine.lock().await;
            get_engine(&mut guard)
                .await?
                .do_action(
                    string_arg(args, "app")?,
                    i64_arg(args, "element_index")?,
                    string_arg(args, "action")?,
                )
                .await?;
            ok_result()
        }
        other => bail!("unknown tool: {other}"),
    }
}

async fn get_engine(slot: &mut Option<Engine>) -> Result<&mut Engine> {
    if slot.is_none() {
        *slot = Some(Engine::new().await?);
    }
    slot.as_mut()
        .context("semantic engine initialization failed")
}

fn string_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    let value = args
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing or invalid string argument {name:?}"))?;
    if name == "app" && value.trim().is_empty() {
        bail!("app must not be empty");
    }
    Ok(value)
}

fn i64_arg(args: &Value, name: &str) -> Result<i64> {
    args.get(name)
        .and_then(Value::as_i64)
        .with_context(|| format!("missing or invalid integer argument {name:?}"))
}

fn number_arg(args: &Value, name: &str) -> Result<f64> {
    let value = args
        .get(name)
        .and_then(Value::as_f64)
        .with_context(|| format!("missing or invalid number argument {name:?}"))?;
    if !value.is_finite() {
        bail!("{name} must be finite");
    }
    Ok(value)
}

fn optional_string<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .with_context(|| format!("invalid string argument {name:?}")),
    }
}

fn optional_i64(args: &Value, name: &str) -> Result<Option<i64>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .with_context(|| format!("invalid integer argument {name:?}")),
    }
}

fn optional_u64(args: &Value, name: &str) -> Result<Option<u64>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .with_context(|| format!("invalid non-negative integer argument {name:?}")),
    }
}

fn optional_number(args: &Value, name: &str) -> Result<Option<f64>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => number_arg(args, name).map(Some),
    }
}

fn optional_coordinate_pair(args: &Value) -> Result<Option<(f64, f64)>> {
    match (optional_number(args, "x")?, optional_number(args, "y")?) {
        (None, None) => Ok(None),
        (Some(x), Some(y)) => Ok(Some((x, y))),
        _ => bail!("x and y must be provided together"),
    }
}

fn reject_coordinates_with_element(args: &Value) -> Result<()> {
    if args.get("x").is_some() || args.get("y").is_some() {
        bail!("element_index cannot be combined with x/y");
    }
    Ok(())
}

fn validate_pages(pages: f64) -> Result<f64> {
    if !pages.is_finite() || pages <= 0.0 || pages > 20.0 {
        bail!("pages must be finite, greater than 0, and at most 20");
    }
    Ok(pages)
}

fn normalize_mouse_button(button: Option<&str>) -> Result<Option<&str>> {
    match button {
        None => Ok(None),
        Some("left" | "l" | "right" | "r" | "middle" | "m") => Ok(button),
        Some(other) => bail!("invalid mouse_button {other:?}"),
    }
}

fn is_left_button(button: Option<&str>) -> bool {
    matches!(button, None | Some("left") | Some("l"))
}

fn normalize_direction(direction: &str) -> Result<&str> {
    match direction {
        "up" | "u" => Ok("up"),
        "down" | "d" => Ok("down"),
        "left" | "l" => Ok("left"),
        "right" | "r" => Ok("right"),
        other => bail!("invalid scroll direction: {other}"),
    }
}

fn insert_optional(map: &mut Map<String, Value>, name: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(name.to_string(), value);
    }
}

fn ok_result() -> Result<Value> {
    text_result("ok")
}

fn text_result(text: impl Into<String>) -> Result<Value> {
    Ok(json!({"content":[{"type":"text","text":text.into()}]}))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let semantic_engine = Mutex::new(None);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let request: Value = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let response = match method {
            "initialize" => json!({
                "jsonrpc":"2.0","id":id,
                "result":{
                    "protocolVersion":request.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
                    "capabilities":{"tools":{}},
                    "serverInfo":{"name":"codex-computer-use-linux","version":"0.1.0"}
                }
            }),
            "tools/list" => {
                json!({"jsonrpc":"2.0","id":id,"result":{"tools":tool_defs()}})
            }
            "tools/call" => {
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                match call_tool(&semantic_engine, name, &arguments).await {
                    Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                    Err(error) => json!({
                        "jsonrpc":"2.0","id":id,
                        "result":{"isError":true,"content":[{"type":"text","text":format!("{error:#}")}]}
                    }),
                }
            }
            "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            _ => json!({
                "jsonrpc":"2.0","id":id,
                "error":{"code":-32601,"message":format!("method not found: {method}")}
            }),
        };
        let mut serialized = serde_json::to_string(&response).unwrap_or_default();
        serialized.push('\n');
        if stdout.write_all(serialized.as_bytes()).is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_argument_is_enforced() {
        assert!(string_arg(&json!({}), "app").is_err());
        assert!(string_arg(&json!({"app":"  "}), "app").is_err());
    }

    #[test]
    fn finite_bounded_pages_only() {
        assert_eq!(validate_pages(1.0).unwrap(), 1.0);
        assert!(validate_pages(0.0).is_err());
        assert!(validate_pages(21.0).is_err());
        assert!(validate_pages(f64::NAN).is_err());
    }

    #[test]
    fn coordinates_are_finite_and_paired() {
        assert_eq!(
            optional_coordinate_pair(&json!({"x":10,"y":20})).unwrap(),
            Some((10.0, 20.0))
        );
        assert!(optional_coordinate_pair(&json!({"x":10})).is_err());
        assert!(number_arg(&json!({"x":f64::INFINITY}), "x").is_err());
    }

    #[test]
    fn exactly_ten_tools_are_exposed() {
        assert_eq!(tool_defs().as_array().unwrap().len(), 10);
    }
}
