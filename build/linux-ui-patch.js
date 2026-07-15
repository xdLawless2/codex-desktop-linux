const fs = require('fs');
const path = require('path');

const MARKER = 'codex-linux-ui-fix';

function patchLinuxUi(appDir) {
  const bootstrapPath = path.join(appDir, '.vite', 'build', 'early-bootstrap.js');
  if (!fs.existsSync(bootstrapPath)) {
    throw new Error(`Expected upstream bootstrap not found: ${bootstrapPath}`);
  }

  let content = fs.readFileSync(bootstrapPath, 'utf8');
  if (content.includes(MARKER)) {
    return;
  }

  // The upstream app creates transparent, always-on-top companion windows for
  // macOS features such as the avatar overlay and quick chat. On KWin Wayland,
  // especially with fractional scaling and NVIDIA, those windows can composite
  // as opaque/corrupted strips over unrelated applications. Suppress only
  // small always-on-top companion windows; leave the primary Codex window
  // untouched. Apply opaque theme surfaces only to normal windows.
  const inject = `(()=>{const {app}=require("electron");const marker="${MARKER}";app.on("browser-window-created",(_event,window)=>{window.setMenuBarVisibility(false);window.autoHideMenuBar=true;const isUnsupportedOverlay=()=>{if(window.isDestroyed()||!window.isAlwaysOnTop())return false;const {width,height}=window.getBounds();const url=window.webContents.getURL();return /avatar.?overlay|quick.?chat|hotkey/i.test(url)||(width<=1200&&height<=600)};const suppressOverlay=()=>{if(!isUnsupportedOverlay())return false;window.hide();window.setIgnoreMouseEvents(true);return true};setImmediate(suppressOverlay);window.on("ready-to-show",suppressOverlay);window.on("show",()=>setImmediate(suppressOverlay));window.webContents.on("did-navigate",suppressOverlay);window.webContents.on("dom-ready",()=>{if(suppressOverlay())return;window.webContents.executeJavaScript(\`(function(){var id="\${marker}";if(document.getElementById(id))return;var s=document.createElement("style");s.id=id;s.textContent="html.electron-dark,html.electron-light{background-color:var(--color-background-surface-under)!important}body{background:var(--color-background-surface-under)!important}.app-shell-left-panel,aside,nav,[class*=sidebar],[class*=Sidebar]{background-color:var(--color-background-surface)!important;transition:none!important;backdrop-filter:none!important}";(document.head||document.documentElement).appendChild(s);})();\`).catch(()=>{});});});})();`;

  // Keep a leading strict-mode directive first so it remains effective.
  const prologue = content.match(/^\s*(["'])use strict\1\s*;?/);
  if (prologue) {
    content = prologue[0] + inject + content.slice(prologue[0].length);
  } else {
    content = inject + content;
  }
  fs.writeFileSync(bootstrapPath, content);
}

module.exports = { patchLinuxUi };

if (require.main === module) {
  const appDir = process.argv[2];
  if (!appDir) {
    throw new Error('Usage: node build/linux-ui-patch.js <app-dir>');
  }
  patchLinuxUi(path.resolve(appDir));
}
