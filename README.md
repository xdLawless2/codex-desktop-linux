<div align="center">

# Codex Desktop for Linux

**Unofficial native Linux packaging for OpenAI Codex Desktop**

[![Platform Support](https://img.shields.io/badge/platform-amd64%20%7C%20arm64-lightgrey?style=flat-square)](#supported-platforms)

Codex Desktop is OpenAI's AI-powered coding agent — shipped as an Electron app with
**no official Linux release**. This project takes the upstream macOS build, patches it for
Linux, and repackages it as a native `.deb` and `.AppImage` — with Wayland support,
rebuilt native modules, sandbox handling, and full desktop integration.

</div>

> [!IMPORTANT]
> **THIS IS AN UNOFFICIAL BUILD. It is not affiliated with, endorsed by, or supported by OpenAI.**
> The git tree contains only packaging and Linux integration source. Locally built packages contain
> an unofficial repackaging of OpenAI's proprietary desktop payload. For the official product, see
> [openai.com/codex](https://openai.com/codex/). Use at your own risk.

---

## ✨ Features

| | Feature | Details |
|---|---|---|
| 🖥️ | **Native packaging** | `.deb` (Debian/Ubuntu) and `.AppImage` (any distro) |
| 🌐 | **Wayland support** | Auto-detects Wayland with native window decorations, falls back to X11 |
| 🏗️ | **Rebuilt native modules** | Compiles `better-sqlite3` and `node-pty` from source for Linux |
| 📦 | **Local packages** | Build `.deb` and `.AppImage` packages from an upstream DMG you download yourself |
| 🛡️ | **Sandbox handling** | Setuid sandbox for DEB; user namespaces required for AppImage |
| 🔧 | **Diagnostics** | Built-in `--doctor` command for troubleshooting |
| 🔗 | **Deep-linking** | `x-scheme-handler/codex` protocol support |
| 🖱️ | **Computer Use (KDE Wayland)** | App-scoped control through KWin UUIDs, AT-SPI semantics, exact-window screenshots, and portal input |
| 🌍 | **Browser Use** | In-app agent browser with a floating picture-in-picture preview, ported from the macOS-only upstream feature |
| 🧮 | **node_repl MCP server** | Linux reimplementation of the bundled `node_repl` agent runtime with the official upstream prompts |
| 🎨 | **System integration** | Desktop entry, icon set, AppStream metainfo |
| 🔐 | **Password storage** | Configurable Chromium backend; defaults to the documented `basic` store |
| 🧹 | **Crash recovery** | Auto-cleans stale `SingletonLock` on startup |

### Supported platforms

- **Architecture:** `x86_64` (amd64) and `arm64`
- **Ubuntu / Debian:** 22.04+ (recommended 24.04+) via `.deb`
- **Any other distro:** via `.AppImage`

---

## ⚡ Installation

This is a source-only repository. It does not publish prebuilt packages or track
OpenAI's DMG, extracted application, proprietary binaries, or generated output.
You download the official upstream DMG and build packages locally.

Install a current `7zip`, Node.js/npm, Rust toolchain, and the
official [Codex CLI](https://developers.openai.com/codex/cli/) first.

```bash
git clone https://github.com/xdLawless2/codex-desktop-linux.git
cd codex-desktop-linux
npm ci
curl -fL "https://persistent.oaistatic.com/codex-app-prod/Codex.dmg" -o Codex.dmg
bash scripts/setup.sh ./Codex.dmg
npm run build:linux
```

Install the locally built Debian package:

```bash
sudo apt install ./dist/codex-desktop-*-x64.deb
```

Or run the locally built AppImage:

```bash
chmod +x dist/codex-desktop-*-x64.AppImage
./dist/codex-desktop-*-x64.AppImage
```

> [!NOTE]
> Local packages include the matching Linux Codex backend. The standalone CLI is
> available from the official [Codex installation documentation](https://developers.openai.com/codex/cli/).

---

## 🎮 Usage

Launch from your app menu, or from a terminal:

```bash
codex-desktop
```

### Diagnostics

```bash
codex-desktop --doctor
```

Prints display server, GPU, sandbox status, CLI resolution, platform info, and Electron version —
the first thing to run when something misbehaves.

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `CODEX_USE_X11` | `0` | Force X11 (`1`) or auto-detect |
| `CODEX_USE_WAYLAND` | `0` | Force Wayland (`1`) or auto-detect |
| `CODEX_DISABLE_VULKAN` | `0` | Disable Vulkan (`1`) |
| `CODEX_PASSWORD_STORE` | `basic` | Chromium password store backend |
| `CODEX_CLI_PATH` | auto | Path to the Codex CLI binary |
| `CODEX_LINUX_BROWSER_USE` | `1` | Disable the Linux Browser Use port (`0`) |
| `CODEX_BROWSER_USE_DEFAULT_VIEWPORT_SIZE` | `1280x800` | In-app browser viewport size |

By default the app inspects `WAYLAND_DISPLAY`: if set, it launches with native Wayland
(including window decorations); otherwise it falls back to X11.

---

## 🏗️ How It Works

Codex Desktop is an **Electron application**. The overwhelming majority of its code is
cross-platform JavaScript, HTML, and CSS living inside an `app.asar` archive — the only
truly platform-specific parts are a couple of native Node modules. That makes it a good
candidate for repackaging: pull the macOS build apart, rebuild the native bits for Linux,
patch a few rough edges, and re-wrap it.

**The packaging pipeline:**

1. **Download** the upstream macOS `.dmg` from OpenAI's CDN
2. **Extract** the `app.asar` and bundled resources (icons, CLI binary, bundled plugins)
3. **Assemble** the Linux agent runtime: a Linux Node.js binary plus this project's
   `node_repl` MCP server, seeded with prompts extracted from the official macOS `node_repl`
4. **Rebuild** native modules (`better-sqlite3`, `node-pty`) for the target Electron version and architecture, with ELF architecture verification
5. **Patch** the app for Linux:
   - Hide the Electron menu bar
   - Suppress unsupported always-on-top companion overlays
   - Inject Linux renderer CSS for opaque window surfaces and sidebar rendering
   - Enable Browser Use on Linux and route PiP frames to the Linux floating preview
6. **Package** as `.deb` / `.AppImage` via `electron-builder`
7. **Install** with proper sandbox permissions and desktop integration

**Linux-specific workarounds applied during install:**

- `chrome-sandbox` is given `chown root:root && chmod 4755` in `postinst`
- AppImage launch fails if unprivileged user namespaces cannot provide Chromium sandboxing
- The password store uses Chromium's `basic` backend
- Stale `SingletonLock` symlinks are cleaned on startup (prevents "app already running" false positives)

---

## 🖱️ Computer Use on Wayland

macOS/Windows ship Computer Use through platform-native accessibility helpers;
the upstream Linux `@oai/sky` backend is X11-only. This project exposes the same
semantic workflow through a Linux MCP server: `list_apps`, `get_app_state`,
indexed/coordinate `click`, `drag`, `scroll`, `press_key`, `type_text`,
`set_value`, `select_text`, and `perform_secondary_action`.

The implementation deliberately has one fail-fast path:

- KWin UUIDs provide exact window discovery, identity, verified activation,
  geometry, PID correlation, and authorized per-window screenshots.
- AT-SPI2 provides roles, names, values, actions, editable text, and stable
  element indices. Steam is launched with Valve's documented accessibility flag.
- Screenshot coordinates are window-relative, bounds-checked, and translated
  only after the exact target window is re-verified. There is no global-desktop
  screenshot or unscoped-coordinate fallback.
- `xdg-desktop-portal` RemoteDesktop performs synthetic pointer/keyboard input
  after target verification. Its one-time session permission is still required.
- Requires KDE Plasma 6 Wayland, KWin ScreenShot2 v5+, AT-SPI2, and a
  RemoteDesktop+ScreenCast portal backend. Unsupported compositors fail clearly.
- The helper is built during setup and registered automatically on first launch.

Run `bash scripts/register-computer-use.sh <helper-dir>` to (re)register manually.

## 🌍 Browser Use on Linux

Upstream ships Browser Use only on macOS: the agent's `node_repl` runtime is a
Mach-O binary and the floating Browser PiP is a native macOS overlay. This
project ports both:

- **`node_repl` MCP server** (`native/node-repl/server.mjs`) — a from-scratch
  Linux reimplementation running on a bundled Linux Node.js runtime. During
  extraction, the official tool prompts are pulled out of the upstream macOS
  binary so the agent sees identical `js`, `js_reset`, and
  `js_add_node_module_dir` tool descriptions. The sandbox is fail-fast:
  untrusted imports are rejected, and `CODEX_HOME` config access is
  path-checked with no direct write path.
- **Browser PiP** (`build/linux-browser-pip.cjs`) — reimplements the floating
  picture-in-picture browser preview with Electron windows, wired into the
  app's PiP frame stream by `build/linux-ui-patch.js`.

Browser Use is enabled by default and can be turned off with
`CODEX_LINUX_BROWSER_USE=0`. CI exercises the `node_repl` MCP protocol
end-to-end via `scripts/internal/test-node-repl.py`.

## Updating

From the repository, one command updates the Linux Codex backend, downloads
the latest official desktop image, rebuilds the native modules and Wayland
Computer Use helpers, reinstalls the local launcher, and produces fresh DEB
and AppImage packages:

```bash
npm run update
```

The updater closes running Codex Desktop processes first. New packages are
written to `dist/`.

### Verify the local build

```bash
bash scripts/smoke-verify.sh
```

---

## 📂 Repository Structure

```
├── .github/workflows/
│   └── ci.yml                    # Source, Rust, workflow, and protocol checks
├── build/
│   ├── after-pack.js             # Packaged launcher and MCP registration
│   ├── linux-browser-pip.cjs     # Linux Browser Use runtime + floating PiP
│   ├── linux-browser-pip-preload.cjs # PiP window preload
│   └── linux-ui-patch.js         # Linux Electron UI behavior
├── native/
│   ├── node-repl/                # Linux node_repl MCP server + prompt extraction
│   └── sky-wayland/              # Rust KWin, AT-SPI, and portal helpers
├── scripts/
│   ├── setup.sh                  # DMG extraction + native rebuild + local launcher
│   ├── update.sh                 # Update CLI/DMG and rebuild everything
│   ├── build-packages.sh         # DEB/AppImage build via electron-builder
│   ├── get-codex-version.sh      # Extract version from DMG
│   ├── register-computer-use.sh  # Register the Linux MCP server
│   ├── smoke-verify.sh           # Post-install smoke test
│   ├── internal/
│   │   ├── extract-dmg.sh        # DMG → app.asar + agent runtime extraction
│   │   ├── build-native.sh       # better-sqlite3 + node-pty rebuild
│   │   ├── build-computer-use.sh # Rust helper build
│   │   └── test-node-repl.py     # node_repl MCP protocol checks (CI)
│   └── debian/
│       ├── postinst              # DEB sandbox and desktop integration
│       └── postrm                # DEB integration cleanup
├── assets/metainfo/              # AppStream metainfo XML
├── docs/computer-use-linux.md    # Computer Use architecture and requirements
├── skills/computer-use-linux/    # Codex Computer Use skill
├── electron-builder.yml          # Packaging configuration
├── upstream-*.txt                # Verified upstream version, digest, and ETag
├── codex-cli-version.txt         # Bundled Linux runtime pin
├── CONTRIBUTING.md
├── SECURITY.md
├── llms.txt
├── LICENSE
└── README.md
```

---

## ⚠️ Notes & Caveats

- This is an **unofficial** project — not affiliated with OpenAI.
- The repository does **not** redistribute Codex source or binaries. Users
  download the upstream `.dmg` and produce packages locally.
- No unsigned APT repository or remote root installer is published.
- Source builds require the official Linux Codex CLI; locally built packages bundle it.

---

## 🙏 Credits

This project stands on the shoulders of the Linux community's earlier work packaging
Electron-based AI desktop apps:

- **[k3d3/claude-desktop-linux-flake](https://github.com/k3d3/claude-desktop-linux-flake)** — Nix flake approach; inspiration for native-addon stubbing and `app.asar` surgery techniques.
- **[aaddrick/claude-desktop-debian](https://github.com/aaddrick/claude-desktop-debian)** — Debian packaging approach; inspiration for Wayland handling and Proxy-based Electron interception.

---

<div align="center">
  <sub>MIT packaging source · See <a href="LICENSE">LICENSE</a> · Codex™ is a trademark of OpenAI</sub>
</div>
