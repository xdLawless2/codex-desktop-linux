const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("codexLinuxPip", {
  onState(callback) {
    ipcRenderer.on("codex-linux-pip:state", (_event, state) => callback(state));
  },
  ready() {
    ipcRenderer.send("codex-linux-pip:ready");
  },
  dismiss(presentationId) {
    ipcRenderer.send("codex-linux-pip:dismiss", presentationId);
  },
  promote(presentationId) {
    ipcRenderer.send("codex-linux-pip:promote", presentationId);
  },
  dragStart(point) {
    ipcRenderer.send("codex-linux-pip:drag-start", point);
  },
  dragMove(point) {
    ipcRenderer.send("codex-linux-pip:drag-move", point);
  },
  dragEnd() {
    ipcRenderer.send("codex-linux-pip:drag-end");
  },
  resizeStart(point) {
    ipcRenderer.send("codex-linux-pip:resize-start", point);
  },
  resizeMove(point) {
    ipcRenderer.send("codex-linux-pip:resize-move", point);
  },
  resizeEnd() {
    ipcRenderer.send("codex-linux-pip:resize-end");
  },
});
