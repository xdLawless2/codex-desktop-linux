# Computer Use on Linux: architecture and contract reference

This documents how Computer Use works on macOS (from the extracted ChatGPT/Codex
app bundle), why the macOS transport cannot be reused on Linux, and the design of
the Linux implementation shipped in this repository.

The goal: give the model the **same semantic, app-scoped Computer Use experience
as macOS** (accessibility tree with stable element indices, per-window
screenshots, indexed actions) over a transport that does **not break on app
updates**.

## 1. How macOS Computer Use works

Three cooperating pieces, all inside `ChatGPT.app`:

1. A signed Swift helper (`Codex Computer Use.app/.../SkyComputerUseService`) that
   reads the macOS accessibility (AX) tree, captures per-app-window screenshots,
   and performs actions. It listens on a Unix socket in a group container.
2. A JS client (`@oai/sky`, inside the bundled `cua_node` Node runtime) exposed to
   the "code mode" node REPL as the global `sky`. On macOS it uses
   `create_client({ target: "mac" })`, which speaks a small framed JSON-RPC
   protocol (`CodexComputerUseIPC-2`) over that socket.
3. A skill (`.codex-plugin/computer-use-node-repl.md`) that teaches the model the
   element-index-first workflow and the confirmation policy.

The model-facing API (from `computer-use-node-repl.md`):

```ts
sky.list_apps(): Promise<App[]>
sky.get_app_state({ app, disableDiff? }): Promise<{ app, screenshot: {url}|null, text }>
sky.click({ app, element_index?, x?, y?, mouse_button?, click_count? })
sky.drag({ app, from_x, from_y, to_x, to_y })
sky.scroll({ app, element_index, direction, pages? })
sky.press_key({ app, key })            // xdotool-style chords, e.g. "super+c"
sky.type_text({ app, text })
sky.set_value({ app, element_index, value })
sky.select_text({ app, element_index, text, prefix?, suffix?, selection_type? })
sky.perform_secondary_action({ app, element_index, action })
```

`get_app_state.text` is a serialized accessibility tree whose lines carry stable
decimal `element_index` values; actions target those indices. The tree is
**diffed server-side** against the previous capture unless `disableDiff: true`.
Coordinate (`x`,`y`) actions are the fallback when no accessibility element is
available (canvas/games).

### The macOS wire protocol (`CodexComputerUseIPC-2`)

Reference only — see `native/sky-wayland` for why we do not implement it as our
transport. Source: `@oai/sky/.../targets/mac/native-pipe.js` + `client.js`.

- Framing: `uint32` little-endian length prefix + UTF-8 JSON payload. Max payload
  8,388,608 bytes. Not newline-delimited; frames may be concatenated per read.
- JSON-RPC 2.0. IDs start at 1 and increment. Method `ping` negotiates the API
  version (`{clientApiVersion}` -> `{serverApiVersion}`, strings must match
  exactly). Method `request` wraps all five request types:
  `{clientApiVersion, codexTurnMetadata, deadlineUnixMilliseconds, request, requestType}`.
- Request types: `ComputerUseIPCListAppsRequest`, `ComputerUseIPCAppPolicyRequest`,
  `ComputerUseIPCAppStartRequest`, `ComputerUseIPCAppGetSkyshotRequest`,
  `ComputerUseIPCAppPerformActionRequest`.
- Action JSON uses Swift-enum tagging, e.g. click:
  `{click:{at:{elementID:{_0:"12"}}|{coordinate:{_0:[x,y]}}, clickCount, mouseButton}}`
  with `mouseButton` 0/1/2 = left/right/middle.
- Errors: JSON-RPC error with negative codes (`-10000` .. `-10020`), e.g.
  `-10008 accessibilityError`, `-10013 incompatibleClientVersion`.

## 2. Why the macOS transport cannot be reused on Linux

The stock macOS code path is unavailable on Linux without patching proprietary,
frequently-updated code:

1. **No Linux `node_repl` with the native pipe.** The Mac client only reaches the
   socket via `globalThis.nodeRepl.nativePipe.createConnection`, a primitive
   injected by the bundled `cua_node/bin/node_repl` runtime. That runtime is
   Mach-O arm64; there is no Linux build. The Linux `codex-code-mode-host` does
   not contain the nativePipe bridge. Without it, `MacNativePipeTransport.create()`
   throws immediately, regardless of any socket we run.
2. **Hard platform gates.** Computer Use is gated to `darwin`/`win32` in the
   renderer (`platform === "macOS" || "windows"`), the main-process plugin
   eligibility list, and node-REPL config construction, plus Statsig flags. These
   live in minified bundles replaced on every update.

Reproducing macOS by patching all of the above would be exactly the fragile,
update-breaking approach to avoid.

## 3. Linux implementation

One codepath. A single Rust engine exposes the macOS semantic surface through an
**MCP server** (`sky_mcp`), which Codex loads as a first-class, versioned public
contract untouched by app updates.

- **Semantics: AT-SPI2** (`org.a11y.Bus`), the Linux analog of the macOS AX API,
  implemented by GTK, Qt/KDE, Firefox, LibreOffice, Chromium/Electron. Provides
  the app list, element trees with roles/names/values/bounds, native actions,
  `EditableText` (for `set_value`), and `Text` selection (for `select_text`).
- **Window identity and focus:** KWin's stable compositor UUID joins window
  discovery, metadata, verified activation, screenshot capture, and PID-based
  correlation to the AT-SPI root. Ambiguous or inaccessible windows fail.
- **Per-window screenshots:** KWin `org.kde.KWin.ScreenShot2.CaptureWindow`
  captures the exact resolved UUID. There is deliberately no full-desktop or
  generic portal fallback for the app-scoped contract.
- **Input injection:** AT-SPI native actions for `element_index` targets (more
  reliable, mirrors macOS `AXPress`); xdg-desktop-portal RemoteDesktop for
  synthetic input only after KWin has activated and verified the exact target.
  Screenshot-relative coordinates are bounds-checked and translated into that
  window's compositor coordinates; raw desktop coordinates are never accepted.
- **Accessibility activation:** the helper registers real AT-SPI events and
  enables the session accessibility status. Apps launched by Computer Use have
  accessibility-safe environments; Steam uses Valve's documented
  `-cef-force-accessibility` flag.
- **Element indices:** the engine assigns stable decimal indices per app capture
  and binds the index map to the exact KWin UUID and screenshot. A changed or
  stale window requires a fresh state capture.

The tool surface, argument names, `App`/`AppState` shapes, key-chord grammar, and
the skill workflow all mirror `computer-use-node-repl.md` so the model operates it
identically to macOS.

## 4. Update-resilience checklist

- Depend only on stable, public contracts: MCP (Codex-native), AT-SPI2, KWin's
  window/scripting/ScreenShot2 D-Bus APIs, and xdg-desktop-portal RemoteDesktop.
  Never patch shipped Electron/Node bundles.
- `scripts/update.sh` re-extracts the DMG, rebuilds the helpers, and re-registers
  the MCP server; nothing depends on macOS-only binaries.
- CI/build asserts the helpers build, the MCP server advertises the full semantic
  tool set, and AT-SPI is reachable at runtime (preflight in the daemon).
