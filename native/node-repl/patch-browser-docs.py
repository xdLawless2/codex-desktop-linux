#!/usr/bin/env python3

import sys
from pathlib import Path


if len(sys.argv) != 2:
    raise SystemExit("usage: patch-browser-docs.py <openai-bundled-directory>")

docs_dir = Path(sys.argv[1]) / "plugins" / "browser" / "docs"

(docs_dir / "visibility.md").write_text(
    """# Browser Visibility Guidance
- Browser Use progress appears automatically in the floating Browser PiP; that is the default presentation.
- Call `visibility.set(true)` to open the full right-hand Browser Pane only when the user explicitly asks to see the full browser view.
- Reuse an existing IAB tab for the same site instead of creating duplicate tabs or fresh sessions. Preserve the browser's persistent cookies, storage, and HTTP cache.
- Avoid rapid navigation retries and reload loops. Wait for the current page or challenge to settle before retrying.
- Submit at most one visual action (click, double-click, drag, keypress, scroll, type, or navigation) per `node_repl` `js` call so the PiP receives a frame for every action.
""",
    encoding="utf-8",
)

(docs_dir / "capabilities" / "browser" / "visibility.md").write_text(
    """# Browser Capability: visibility
The floating Browser PiP is automatic and independent of this capability. `set(true)` opens the full right-hand Browser Pane; use it only when the user explicitly asks for the full browser view.

```ts
const capability = await browser.capabilities.get("visibility");

interface VisibilityBrowserCapability {
  get(): Promise<boolean>; // Read whether the full Browser Pane is open.
  set(visible: boolean): Promise<void>; // Open or close the full Browser Pane.
}
```
""",
    encoding="utf-8",
)
