const electron = require("electron");
const path = require("node:path");

const { app, ipcMain, session, WebContentsView } = electron;
const NativeBrowserWindow = electron.BrowserWindow;

const PIP_SIZE = 304;
const PIP_MIN_SIZE = 240;
const PIP_MAX_SIZE = 720;
const PIP_MARGIN = 18;
const PIP_TOP = 52;
const IAB_PARTITION = "persist:codex-browser-app";
const IAB_PARTITION_DIR = "codex-browser-app";
const MESSAGE_CHANNEL = "codex_desktop:message-for-view";
const MAX_IMAGE_DATA_URL_LENGTH = 20 * 1024 * 1024;
const IMAGE_DATA_URL_PATTERN = /^data:image\/(?:png|jpeg|webp);base64,/u;
const preloadPath = path.join(__dirname, "linux-browser-pip-preload.cjs");

const linuxBrowserUseDefaults = {
  browserPane: true,
  cuaPIP: true,
  inAppBrowserUse: true,
  inAppBrowserUseAllowed: true,
};

let configuredFeatures = {};
try {
  configuredFeatures = JSON.parse(
    process.env.CODEX_ELECTRON_DESKTOP_FEATURE_OVERRIDES || "{}",
  );
  if (
    !configuredFeatures ||
    typeof configuredFeatures !== "object" ||
    Array.isArray(configuredFeatures)
  ) {
    throw new TypeError("feature overrides must be a JSON object");
  }
} catch (error) {
  console.warn(
    "[linux-browser-pip] ignoring invalid CODEX_ELECTRON_DESKTOP_FEATURE_OVERRIDES:",
    error instanceof Error ? error.message : String(error),
  );
  configuredFeatures = {};
}

const browserUseEnabled = process.env.CODEX_LINUX_BROWSER_USE !== "0";
process.env.CODEX_ELECTRON_DESKTOP_FEATURE_OVERRIDES = JSON.stringify(
  browserUseEnabled
    ? { ...linuxBrowserUseDefaults, ...configuredFeatures }
    : configuredFeatures,
);

const packageJson = require(path.join(app.getAppPath(), "package.json"));
process.env.BUILD_FLAVOR ||= packageJson.buildFlavor || "prod";
process.env.CODEX_BUILD_NUMBER ||= packageJson.version;

// IAB pages use the base partition or per-route partitions derived from it
// (persist:codex-browser-app-route:...). Both map to storage directories
// containing the base name.
function isIabSession(browserSession) {
  return (
    typeof browserSession.storagePath === "string" &&
    browserSession.storagePath.includes(IAB_PARTITION_DIR)
  );
}

function configureIabProfile(browserSession) {
  const upstreamUserAgent = browserSession.getUserAgent();
  const userAgent = upstreamUserAgent.replace(/\sElectron\/[^\s]+/u, "");

  const languages = [...new Set(app.getPreferredSystemLanguages())];
  if (languages.length === 0) {
    throw new Error("Expected at least one preferred system language");
  }
  const acceptLanguages = languages
    .map((language, index) =>
      index === 0
        ? language
        : `${language};q=${Math.max(0.1, 1 - index * 0.1).toFixed(1)}`,
    )
    .join(",");

  browserSession.setUserAgent(userAgent, acceptLanguages);
  console.log(
    "[linux-browser-profile] persistent IAB profile and stable identity enabled",
  );
}

app.once("ready", () => configureIabProfile(session.fromPartition(IAB_PARTITION)));
app.on("session-created", browserSession => {
  if (isIabSession(browserSession)) configureIabProfile(browserSession);
});

function trackIabGuest(contents) {
  if (pipView && contents === pipView.webContents) return;
  if (!isIabSession(contents.session)) return;
  const record = { contents };
  const markActive = () => {
    if (activePresentationId) startFramePump();
  };
  iabGuests.set(contents.id, record);
  // macOS navigates IAB history with a trackpad swipe (Electron's `swipe`
  // event is darwin-only). Provide the Linux convention instead.
  contents.on("before-input-event", (event, input) => {
    if (input.type !== "keyDown" || !input.alt || input.control || input.meta) {
      return;
    }
    const history = contents.navigationHistory;
    if (input.key === "ArrowLeft" && history.canGoBack()) {
      event.preventDefault();
      history.goBack();
    } else if (input.key === "ArrowRight" && history.canGoForward()) {
      event.preventDefault();
      history.goForward();
    }
  });
  for (const event of [
    "before-input-event",
    "did-start-navigation",
    "did-navigate",
    "did-navigate-in-page",
    "dom-ready",
    "focus",
    "media-started-playing",
  ]) {
    contents.on(event, markActive);
  }
  contents.once("destroyed", () => iabGuests.delete(contents.id));
}

app.on("web-contents-created", (_event, contents) => {
  trackIabGuest(contents);
});

let primaryWindow = null;
let pipView = null;
let pipOwner = null;
let pipBounds = null;
let dragState = null;
let resizeState = null;
let subscribedGuest = null;
let screencastMessageHandler = null;
let activePresentationId = null;
let browserSidebarManager = null;

const presentations = new Map();
const cursorStates = new Map();
const suppressedThreadIds = new Set();
const suppressedOverlayWindows = new WeakSet();
const boundOwnerWindows = new WeakSet();
const iabGuests = new Map();

function isUnsupportedAvatarOverlay(window, navigationUrl) {
  if (
    window.isDestroyed() ||
    window.getParentWindow() ||
    !window.isAlwaysOnTop()
  ) {
    return false;
  }

  const url = navigationUrl || window.webContents.getURL();
  try {
    const parsed = new URL(url);
    const route = `${parsed.pathname}${parsed.hash}`;
    return (
      !route.includes("avatar-overlay-composition-surface") &&
      /(?:^|\/)avatar-overlay(?:$|[/?#])/u.test(route)
    );
  } catch {
    return false;
  }
}

function suppressUpstreamOverlay(window) {
  suppressedOverlayWindows.add(window);
  if (window.isDestroyed()) return;
  window.setAlwaysOnTop(false);
  window.setSkipTaskbar(true);
  window.setIgnoreMouseEvents(true);
  window.hide();
  console.log("[linux-browser-pip] suppressed unsupported avatar overlay");
}

function pipDocument() {
  return `<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta
    http-equiv="Content-Security-Policy"
    content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; script-src 'unsafe-inline'"
  >
  <style>
    :root {
      color-scheme: dark;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }
    * { box-sizing: border-box; }
    html, body {
      width: 100%;
      height: 100%;
      margin: 0;
      overflow: hidden;
      background: transparent;
      user-select: none;
    }
    #stack {
      position: absolute;
      inset: 0;
    }
    .card {
      --depth: 0;
      position: absolute;
      right: calc(18px + var(--depth) * 13px);
      top: calc(44px - var(--depth) * 11px);
      width: 250px;
      height: 178px;
      overflow: hidden;
      border: 1px solid rgba(255, 255, 255, .24);
      border-radius: 14px;
      background: #fff;
      box-shadow:
        0 22px 55px rgba(0, 0, 0, .38),
        0 3px 12px rgba(0, 0, 0, .24);
      opacity: calc(1 - var(--depth) * .12);
      transform:
        scale(calc(1 - var(--depth) * .025))
        rotate(calc(var(--depth) * -1.1deg));
      transform-origin: bottom right;
      transition:
        right 180ms cubic-bezier(.2, .8, .2, 1),
        top 180ms cubic-bezier(.2, .8, .2, 1),
        width 180ms cubic-bezier(.2, .8, .2, 1),
        height 180ms cubic-bezier(.2, .8, .2, 1),
        opacity 180ms ease,
        transform 180ms cubic-bezier(.2, .8, .2, 1);
    }
    .card.front {
      cursor: grab;
      touch-action: none;
    }
    .card.front.dragging {
      cursor: grabbing;
    }
    .card.entering {
      animation: card-in 240ms cubic-bezier(.18, .9, .24, 1.08) both;
    }
    @keyframes card-in {
      from { opacity: 0; transform: translate(18px, 16px) scale(.88); }
    }
    .card img {
      display: block;
      width: 100%;
      height: 100%;
      object-fit: cover;
      pointer-events: none;
    }
    .shade {
      position: absolute;
      inset: 0;
      opacity: 0;
      background: linear-gradient(
        180deg,
        rgba(0, 0, 0, .48) 0,
        rgba(0, 0, 0, 0) 38%,
        rgba(0, 0, 0, .12) 100%
      );
      transition: opacity 120ms ease;
      pointer-events: none;
    }
    .card.front:hover .shade,
    .card.front:hover .controls,
    .card.front:hover .resize-handle { opacity: 1; }
    .controls {
      position: absolute;
      top: 9px;
      right: 9px;
      display: flex;
      gap: 7px;
      opacity: 0;
      transition: opacity 120ms ease;
    }
    button {
      display: grid;
      width: 29px;
      height: 29px;
      padding: 0;
      place-items: center;
      border: 1px solid rgba(255, 255, 255, .28);
      border-radius: 999px;
      color: white;
      background: rgba(24, 24, 27, .76);
      box-shadow: 0 2px 7px rgba(0, 0, 0, .28);
      font: 600 17px/1 sans-serif;
      cursor: pointer;
      backdrop-filter: blur(12px);
    }
    button:hover { background: rgba(38, 38, 42, .92); }
    .resize-handle {
      position: absolute;
      left: 7px;
      bottom: 7px;
      z-index: 5;
      width: 24px;
      height: 24px;
      border-left: 2px solid rgba(255, 255, 255, .9);
      border-bottom: 2px solid rgba(255, 255, 255, .9);
      border-bottom-left-radius: 5px;
      opacity: .65;
      cursor: nesw-resize;
      transition: opacity 120ms ease;
      filter: drop-shadow(0 1px 2px rgba(0, 0, 0, .65));
    }
    .agent-cursor {
      position: absolute;
      z-index: 4;
      width: 25px;
      height: 31px;
      pointer-events: none;
      transform: translate(-3px, -2px);
      transition:
        left 80ms linear,
        top 80ms linear;
      filter: drop-shadow(0 2px 2px rgba(0, 0, 0, .42));
    }
    .agent-cursor svg {
      display: block;
      width: 100%;
      height: 100%;
    }
    .agent-cursor::after {
      content: "";
      position: absolute;
      left: 2px;
      top: 2px;
      width: 18px;
      height: 18px;
      border: 2px solid rgba(87, 159, 255, .9);
      border-radius: 999px;
      opacity: 0;
    }
    .agent-cursor.moving::after {
      animation: cursor-pulse 360ms ease-out both;
    }
    @keyframes cursor-pulse {
      from { opacity: .9; transform: scale(.35); }
      to { opacity: 0; transform: scale(1.65); }
    }
  </style>
</head>
<body>
  <div id="stack"></div>
  <script>
    const stack = document.getElementById("stack");
    let previousFrontId = null;
    let previousOrder = [];
    let lastState = { presentations: [] };

    function fitCard(card, image) {
      const ratio = image.naturalWidth / image.naturalHeight;
      const maxSize = Math.max(
        186,
        Math.min(window.innerWidth, window.innerHeight) - 54,
      );
      let width = maxSize;
      let height = Math.round(width / ratio);
      if (height > maxSize) {
        height = maxSize;
        width = Math.round(height * ratio);
      }
      card.style.width = width + "px";
      card.style.height = height + "px";
    }

    function control(label, className, text, onClick) {
      const button = document.createElement("button");
      button.className = className;
      button.type = "button";
      button.title = label;
      button.setAttribute("aria-label", label);
      button.textContent = text;
      button.addEventListener("pointerdown", event => event.stopPropagation());
      button.addEventListener("click", event => {
        event.stopPropagation();
        onClick();
      });
      return button;
    }

    // The main process resizes/moves this view while the pointer is down,
    // which makes Chromium drop element pointer capture. Track active
    // gestures with window-level listeners instead.
    function trackGesture(onMove, onEnd) {
      const move = event => onMove(event);
      const end = event => {
        window.removeEventListener("pointermove", move, true);
        window.removeEventListener("pointerup", end, true);
        window.removeEventListener("pointercancel", end, true);
        onEnd(event);
      };
      window.addEventListener("pointermove", move, true);
      window.addEventListener("pointerup", end, true);
      window.addEventListener("pointercancel", end, true);
    }

    function attachCardInteraction(card) {
      const dragThreshold = 4;

      card.addEventListener("pointerdown", event => {
        if (event.button !== 0) return;
        const start = { x: event.screenX, y: event.screenY };
        let dragging = false;
        trackGesture(
          moveEvent => {
            if (!dragging) {
              const moved = Math.hypot(
                moveEvent.screenX - start.x,
                moveEvent.screenY - start.y,
              );
              if (moved < dragThreshold) return;
              dragging = true;
              card.classList.add("dragging");
              window.codexLinuxPip.dragStart(start);
            }
            window.codexLinuxPip.dragMove({
              x: moveEvent.screenX,
              y: moveEvent.screenY,
            });
          },
          () => {
            card.classList.remove("dragging");
            if (dragging) window.codexLinuxPip.dragEnd();
          },
        );
      });
    }

    function appendResizeHandle(card) {
      const handle = document.createElement("div");
      handle.className = "resize-handle";

      handle.addEventListener("pointerdown", event => {
        if (event.button !== 0) return;
        event.stopPropagation();
        window.codexLinuxPip.resizeStart({
          x: event.screenX,
          y: event.screenY,
        });
        trackGesture(
          moveEvent => {
            window.codexLinuxPip.resizeMove({
              x: moveEvent.screenX,
              y: moveEvent.screenY,
            });
          },
          () => window.codexLinuxPip.resizeEnd(),
        );
      });
      card.append(handle);
    }

    function updateCursor(card, image, cursor) {
      let pointer = card.querySelector(".agent-cursor");
      if (cursor?.visible !== true) {
        pointer?.remove();
        return;
      }
      const update = () => {
        if (!image.naturalWidth || !image.naturalHeight) return;
        const sourceWidth = cursor.viewport?.width ?? image.naturalWidth;
        const sourceHeight = cursor.viewport?.height ?? image.naturalHeight;
        if (!pointer) {
          pointer = document.createElement("div");
          pointer.className = "agent-cursor";
          pointer.innerHTML =
            '<svg viewBox="0 0 25 31" aria-hidden="true">' +
            '<path d="M3 2.5v21.2l5.8-5.2 4.1 9.1 4.3-2-4.1-8.8h8.2L3 2.5Z" ' +
            'fill="#4f9cff" stroke="white" stroke-width="2.2" stroke-linejoin="round"/>' +
            '</svg>';
          card.append(pointer);
        }
        pointer.style.left = cursor.x / sourceWidth * 100 + "%";
        pointer.style.top = cursor.y / sourceHeight * 100 + "%";
        const sequence = String(cursor.moveSequence ?? "");
        if (pointer.dataset.moveSequence !== sequence) {
          pointer.dataset.moveSequence = sequence;
          pointer.classList.remove("moving");
          void pointer.offsetWidth;
          pointer.classList.add("moving");
        }
      };
      if (image.complete) update();
      else image.addEventListener("load", update, { once: true });
    }

    function render(state) {
      lastState = state;
      const visible = state.presentations.slice(0, 3);
      const existingCards = new Map(
        [...stack.querySelectorAll(".card")].map(card => [
          card.dataset.presentationId,
          card,
        ]),
      );
      const samePresentations =
        previousOrder.length === visible.length &&
        visible.every(
          (presentation, index) => presentation.id === previousOrder[index],
        );

      if (samePresentations) {
        for (let depth = 0; depth < visible.length; depth += 1) {
          const presentation = visible[depth];
          const card = existingCards.get(presentation.id);
          const image = card.querySelector("img");
          if (image.src !== presentation.dataUrl) {
            image.src = presentation.dataUrl;
          }
          if (depth === 0) updateCursor(card, image, presentation.cursor);
        }
        return;
      }

      stack.replaceChildren();

      for (let depth = visible.length - 1; depth >= 0; depth -= 1) {
        const presentation = visible[depth];
        const card = document.createElement("div");
        card.className = "card" + (depth === 0 ? " front" : "");
        card.dataset.presentationId = presentation.id;
        if (depth === 0 && presentation.id !== previousFrontId) {
          card.classList.add("entering");
        }
        card.style.setProperty("--depth", depth);
        card.style.zIndex = String(visible.length - depth);

        const image = document.createElement("img");
        image.alt = "";
        image.draggable = false;
        image.addEventListener("load", () => fitCard(card, image));
        image.src = presentation.dataUrl;
        card.append(image);

        if (depth === 0) {
          updateCursor(card, image, presentation.cursor);
          attachCardInteraction(card);

          const shade = document.createElement("div");
          shade.className = "shade";
          card.append(shade);

          const controls = document.createElement("div");
          controls.className = "controls";
          controls.append(
            control("Close preview", "close", "×", () => {
              window.codexLinuxPip.dismiss(presentation.id);
            }),
          );
          card.append(controls);
          appendResizeHandle(card);
        } else {
          card.addEventListener("click", () => {
            window.codexLinuxPip.promote(presentation.id);
          });
        }

        stack.append(card);
      }

      previousFrontId = visible[0]?.id ?? null;
      previousOrder = visible.map(presentation => presentation.id);
    }

    window.codexLinuxPip.onState(render);
    window.addEventListener("resize", () => {
      for (const card of stack.querySelectorAll(".card")) {
        fitCard(card, card.querySelector("img"));
      }
      render(lastState);
    });
    window.codexLinuxPip.ready();
  </script>
</body>
</html>`;
}

function presentationParts(presentationId) {
  if (
    typeof presentationId !== "string" ||
    !presentationId.startsWith("browser:") ||
    presentationId.length > 4096
  ) {
    return null;
  }

  try {
    const parts = JSON.parse(presentationId.slice("browser:".length));
    if (
      !Array.isArray(parts) ||
      parts.length !== 3 ||
      parts.some(part => typeof part !== "string" || part.length === 0)
    ) {
      return null;
    }
    const [threadId, browserId, browserTabId] = parts;
    return { threadId, browserId, browserTabId };
  } catch {
    return null;
  }
}

function currentOwner() {
  if (primaryWindow && !primaryWindow.isDestroyed()) return primaryWindow;
  return NativeBrowserWindow.getAllWindows().find(
    window =>
      !suppressedOverlayWindows.has(window) &&
      !window.isDestroyed() &&
      !window.getParentWindow(),
  );
}

function clampBounds(bounds) {
  if (!pipOwner || pipOwner.isDestroyed()) return bounds;
  const content = pipOwner.getContentBounds();
  const size = Math.min(
    bounds.width,
    bounds.height,
    content.width,
    content.height,
  );
  return {
    x: Math.max(0, Math.min(bounds.x, content.width - size)),
    y: Math.max(0, Math.min(bounds.y, content.height - size)),
    width: size,
    height: size,
  };
}

function placePip() {
  if (!pipView || !pipOwner || pipOwner.isDestroyed()) return;
  const content = pipOwner.getContentBounds();
  pipBounds = clampBounds(
    pipBounds ?? {
      x: content.width - PIP_SIZE - PIP_MARGIN,
      y: PIP_TOP,
      width: PIP_SIZE,
      height: PIP_SIZE,
    },
  );
  pipView.setBounds(pipBounds);
}

function orderedPresentations() {
  return [...presentations.values()].reverse().map(presentation => {
    const { threadId } = presentationParts(presentation.id);
    return {
      ...presentation,
      cursor: cursorStates.get(threadId) ?? null,
    };
  });
}

function sendState() {
  if (!pipView || pipView.webContents.isDestroyed()) return;
  pipView.webContents.send("codex-linux-pip:state", {
    presentations: orderedPresentations(),
  });
}

function activeIabPageState() {
  if (!browserSidebarManager || !activePresentationId) return null;
  const { threadId, browserTabId } = presentationParts(activePresentationId);
  const live = [];
  for (const { contents } of iabGuests.values()) {
    if (contents.isDestroyed()) continue;
    const state = browserSidebarManager.findPageStateForWebContentsId(
      contents.id,
    );
    if (!state) continue;
    if (
      state.conversationId === threadId &&
      String(state.page.browserTabId) === String(browserTabId)
    ) {
      return { contents, state };
    }
    live.push({ contents, state });
  }
  // Renderer-side thread states may be keyed by client aliases
  // (client-new-thread:...) that never equal the server thread id carried by
  // PiP presentations. With a single live IAB page there is no ambiguity.
  return live.length === 1 ? live[0] : null;
}

function activeIabGuest() {
  return activeIabPageState()?.contents ?? null;
}

function activePaneExpanded() {
  // Upstream's own "pane is presented" formula; visible/bounds live on the
  // thread state, not the page record.
  const threadState = activeIabPageState()?.state.threadState;
  return threadState?.visible === true && threadState.bounds != null;
}

async function startFramePump() {
  const guest = activeIabGuest();
  if (!guest || guest === subscribedGuest) return;
  stopFramePump();
  subscribedGuest = guest;
  screencastMessageHandler = (_event, method, params) => {
    if (method !== "Page.screencastFrame") return;
    guest.debugger
      .sendCommand("Page.screencastFrameAck", {
        sessionId: params.sessionId,
      })
      .catch(() => {});
    if (activePresentationId && shouldShowPip()) {
      const presentation = presentations.get(activePresentationId);
      if (presentation) {
        presentation.dataUrl = `data:image/jpeg;base64,${params.data}`;
        sendState();
      }
    }
  };
  guest.debugger.on("message", screencastMessageHandler);
  try {
    guest.debugger.attach("1.3");
    await guest.debugger.sendCommand("Page.enable");
    await guest.debugger.sendCommand("Page.startScreencast", {
      format: "jpeg",
      quality: 84,
      maxWidth: PIP_MAX_SIZE,
      maxHeight: PIP_MAX_SIZE,
      everyNthFrame: 1,
    });
  } catch (error) {
    console.error("[linux-browser-pip] cannot start live screencast", error);
    stopFramePump();
  }
}

function stopFramePump() {
  if (subscribedGuest && !subscribedGuest.isDestroyed()) {
    if (screencastMessageHandler) {
      subscribedGuest.debugger.off("message", screencastMessageHandler);
    }
    if (subscribedGuest.debugger.isAttached()) {
      subscribedGuest.debugger.sendCommand("Page.stopScreencast").catch(() => {});
      subscribedGuest.debugger.detach();
    }
  }
  subscribedGuest = null;
  screencastMessageHandler = null;
}

function shouldShowPip() {
  if (!activePresentationId || presentations.size === 0) return false;
  const { threadId } = presentationParts(activePresentationId);
  return !suppressedThreadIds.has(threadId) && !activePaneExpanded();
}

function syncVisibility() {
  if (!pipView) return;
  pipView.setVisible(
    shouldShowPip() &&
      pipOwner != null &&
      !pipOwner.isDestroyed() &&
      pipOwner.isVisible() &&
      !pipOwner.isMinimized(),
  );
}

function bindOwner(owner) {
  if (boundOwnerWindows.has(owner)) return;
  boundOwnerWindows.add(owner);
  owner.on("resize", placePip);
  owner.on("minimize", syncVisibility);
  owner.on("hide", syncVisibility);
  owner.on("restore", syncVisibility);
  owner.on("show", syncVisibility);
  owner.on("closed", destroyPip);
}

function ensurePip() {
  if (pipView && !pipView.webContents.isDestroyed()) return pipView;

  const owner = currentOwner();
  if (!owner) throw new Error("Cannot create browser PiP without an owner window");

  pipView = new WebContentsView({
    webPreferences: {
      preload: preloadPath,
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      backgroundThrottling: false,
    },
  });
  pipOwner = owner;
  pipBounds = null;
  owner.contentView.addChildView(pipView);
  pipView.setBackgroundColor("#00000000");
  pipView.setVisible(false);
  bindOwner(owner);

  pipView.webContents.on("did-finish-load", () => {
    sendState();
    syncVisibility();
  });
  pipView.webContents.loadURL(
    `data:text/html;charset=UTF-8,${encodeURIComponent(pipDocument())}`,
  );

  placePip();
  return pipView;
}

function broadcastSuppressedThreads() {
  const owner = currentOwner();
  if (owner && !owner.isDestroyed() && !owner.webContents.isDestroyed()) {
    owner.webContents.send(MESSAGE_CHANNEL, {
      type: "remote-hosted-pip-hidden-thread-ids-requested",
      hiddenThreadIds: [...suppressedThreadIds],
    });
  }
}

function dismissPresentation(presentationId) {
  const parts = presentationParts(presentationId);
  if (!parts || !presentations.has(presentationId)) return false;
  suppressedThreadIds.add(parts.threadId);
  broadcastSuppressedThreads();
  syncVisibility();
  return true;
}

function activatePresentation(presentationId) {
  const presentation = presentations.get(presentationId);
  if (!presentation) return false;
  presentations.delete(presentationId);
  presentations.set(presentationId, presentation);
  activePresentationId = presentationId;
  startFramePump();
  sendState();
  return true;
}

function syncSuppressedThreads(hiddenThreadIds) {
  if (
    !Array.isArray(hiddenThreadIds) ||
    hiddenThreadIds.some(
      threadId => typeof threadId !== "string" || threadId.length === 0,
    )
  ) {
    return false;
  }
  suppressedThreadIds.clear();
  for (const threadId of hiddenThreadIds) suppressedThreadIds.add(threadId);
  syncVisibility();
  return true;
}

function upsertPresentation(metadata) {
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) {
    return false;
  }
  const { presentationId: id, dataUrl: data } = metadata;
  const parts = presentationParts(id);
  if (
    !parts ||
    typeof data !== "string" ||
    data.length > MAX_IMAGE_DATA_URL_LENGTH ||
    !IMAGE_DATA_URL_PATTERN.test(data)
  ) {
    return false;
  }

  // macOS semantics: a fresh screenshot for the thread lifts its suppression.
  suppressedThreadIds.delete(parts.threadId);
  presentations.delete(id);
  presentations.set(id, { id, dataUrl: data });
  activePresentationId = id;

  ensurePip();
  startFramePump();
  sendState();
  syncVisibility();
  console.log("[linux-browser-pip] presented Browser Use screenshot", id);
  return true;
}

function invalidatePresentation(id) {
  if (!presentationParts(id)) return false;
  presentations.delete(id);
  if (activePresentationId === id) {
    activePresentationId = presentations.size
      ? [...presentations.keys()].at(-1)
      : null;
  }
  if (activePresentationId) startFramePump();
  else stopFramePump();
  sendState();
  syncVisibility();
  return true;
}

function updateCursor(metadata) {
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) {
    return false;
  }
  const { conversationId, visible, x, y, moveSequence, viewport } = metadata;
  if (
    typeof conversationId !== "string" ||
    conversationId.length === 0 ||
    typeof visible !== "boolean"
  ) {
    return false;
  }
  if (
    visible &&
    (!Number.isFinite(x) ||
      !Number.isFinite(y) ||
      (viewport != null &&
        (!Number.isFinite(viewport.width) ||
          !Number.isFinite(viewport.height) ||
          viewport.width <= 0 ||
          viewport.height <= 0)))
  ) {
    return false;
  }

  if (visible) {
    cursorStates.set(conversationId, {
      visible,
      x,
      y,
      moveSequence,
      viewport,
    });
  } else {
    cursorStates.delete(conversationId);
  }
  sendState();
  return true;
}

function syncBrowserPanel(metadata, manager) {
  browserSidebarManager = manager;
  if (activePresentationId) startFramePump();
  // macOS semantics: while a thread's Browser Pane is expanded its PiP is
  // hidden; closing the pane brings the PiP back. Pane visibility is read
  // from the sidebar manager's page state in shouldShowPip().
  syncVisibility();
  return true;
}

function destroyPip() {
  stopFramePump();
  presentations.clear();
  cursorStates.clear();
  suppressedThreadIds.clear();
  browserSidebarManager = null;
  activePresentationId = null;
  dragState = null;
  resizeState = null;
  if (pipView) {
    if (pipOwner && !pipOwner.isDestroyed()) {
      pipOwner.contentView.removeChildView(pipView);
    }
    if (!pipView.webContents.isDestroyed()) pipView.webContents.close();
  }
  pipView = null;
  pipOwner = null;
  pipBounds = null;
}

ipcMain.on("codex-linux-pip:dismiss", (event, presentationId) => {
  if (pipView && event.sender === pipView.webContents) {
    dismissPresentation(presentationId);
  }
});
ipcMain.on("codex-linux-pip:ready", event => {
  if (pipView && event.sender === pipView.webContents) sendState();
});
ipcMain.on("codex-linux-pip:promote", (event, presentationId) => {
  if (
    !pipView ||
    event.sender !== pipView.webContents ||
    !presentations.has(presentationId)
  ) {
    return;
  }
  activatePresentation(presentationId);
  syncVisibility();
});
ipcMain.on("codex-linux-pip:drag-start", (event, point) => {
  if (
    !pipView ||
    event.sender !== pipView.webContents ||
    !pipBounds ||
    !Number.isFinite(point?.x) ||
    !Number.isFinite(point?.y)
  ) {
    return;
  }
  resizeState = null;
  dragState = { startPoint: point, startBounds: { ...pipBounds } };
});
ipcMain.on("codex-linux-pip:drag-move", (event, point) => {
  if (
    !pipView ||
    event.sender !== pipView.webContents ||
    !dragState ||
    !Number.isFinite(point?.x) ||
    !Number.isFinite(point?.y)
  ) {
    return;
  }
  pipBounds = clampBounds({
    ...dragState.startBounds,
    x: Math.round(
      dragState.startBounds.x + (point.x - dragState.startPoint.x),
    ),
    y: Math.round(
      dragState.startBounds.y + (point.y - dragState.startPoint.y),
    ),
  });
  pipView.setBounds(pipBounds);
});
ipcMain.on("codex-linux-pip:drag-end", event => {
  if (pipView && event.sender === pipView.webContents) {
    dragState = null;
  }
});
ipcMain.on("codex-linux-pip:resize-start", (event, point) => {
  if (
    !pipView ||
    event.sender !== pipView.webContents ||
    !pipBounds ||
    !Number.isFinite(point?.x) ||
    !Number.isFinite(point?.y)
  ) {
    return;
  }
  dragState = null;
  resizeState = { startPoint: point, startBounds: { ...pipBounds } };
});
ipcMain.on("codex-linux-pip:resize-move", (event, point) => {
  if (
    !pipView ||
    event.sender !== pipView.webContents ||
    !resizeState ||
    !Number.isFinite(point?.x) ||
    !Number.isFinite(point?.y)
  ) {
    return;
  }
  // Bottom-left handle: dragging away from the top-right corner grows the view.
  const deltaX = point.x - resizeState.startPoint.x;
  const deltaY = point.y - resizeState.startPoint.y;
  const amount = Math.abs(deltaX) >= Math.abs(deltaY) ? -deltaX : deltaY;
  const right = resizeState.startBounds.x + resizeState.startBounds.width;
  const size = Math.round(
    Math.max(
      PIP_MIN_SIZE,
      Math.min(PIP_MAX_SIZE, resizeState.startBounds.width + amount),
    ),
  );
  pipBounds = clampBounds({
    x: right - size,
    y: resizeState.startBounds.y,
    width: size,
    height: size,
  });
  pipView.setBounds(pipBounds);
});
ipcMain.on("codex-linux-pip:resize-end", event => {
  if (pipView && event.sender === pipView.webContents) {
    resizeState = null;
  }
});

globalThis.__codexLinuxPipUpsert = upsertPresentation;
globalThis.__codexLinuxPipInvalidate = invalidatePresentation;
globalThis.__codexLinuxPipCursor = updateCursor;
globalThis.__codexLinuxPipPanelSync = syncBrowserPanel;
globalThis.__codexLinuxPipHiddenThreads = syncSuppressedThreads;

app.on("browser-window-created", (_event, window) => {
  const classifyWindow = navigationUrl => {
    if (window.isDestroyed() || suppressedOverlayWindows.has(window)) return true;
    if (isUnsupportedAvatarOverlay(window, navigationUrl)) {
      suppressUpstreamOverlay(window);
      return true;
    }
    return false;
  };

  window.webContents.on(
    "did-start-navigation",
    (_navigationEvent, navigationUrl, _isInPlace, isMainFrame) => {
      if (isMainFrame) classifyWindow(navigationUrl);
    },
  );
  window.webContents.once("did-finish-load", () => classifyWindow());
  queueMicrotask(() => {
    if (classifyWindow()) return;
    if (!window.getParentWindow()) primaryWindow = window;
  });
});
app.on("before-quit", destroyPip);

console.log("[linux-browser-pip] Codex-owned browser PiP enabled");
