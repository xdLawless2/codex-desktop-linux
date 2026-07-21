const fs = require('fs');
const path = require('path');

const MARKER = 'codex-linux-ui-fix-v2';
const RUNTIME_NAME = 'linux-browser-pip.cjs';
const PRELOAD_NAME = 'linux-browser-pip-preload.cjs';

function patchProductionFeatureOverrides(appDir) {
  const buildDir = path.join(appDir, '.vite', 'build');
  const candidates = fs
    .readdirSync(buildDir)
    .filter((name) => /^main-.*\.js$/.test(name))
    .map((name) => path.join(buildDir, name))
    .filter((file) =>
      fs.readFileSync(file, 'utf8').includes('CODEX_ELECTRON_DESKTOP_FEATURE_OVERRIDES'),
    );
  if (candidates.length !== 1) {
    throw new Error(`Expected one desktop feature bundle, found ${candidates.length}`);
  }

  const featureBundlePath = candidates[0];
  let content = fs.readFileSync(featureBundlePath, 'utf8');
  const linuxGuard =
    /([A-Za-z_$][\w$]*)=r===`linux`\?([A-Za-z_$][\w$]*)\(n\):t===([A-Za-z_$][\w$]*)\.a\.Dev\?\2\(n\):null;return \1==null\?/;
  if (!linuxGuard.test(content)) {
    const devGuard =
      /([A-Za-z_$][\w$]*)=t===([A-Za-z_$][\w$]*)\.a\.Dev\?([A-Za-z_$][\w$]*)\(n\):null;return \1==null\?/;
    if (!devGuard.test(content)) {
      throw new Error('Expected desktop feature override guard not found');
    }
    content = content.replace(
      devGuard,
      (_match, value, buildFlavor, readOverrides) =>
        `${value}=r===\`linux\`?${readOverrides}(n):t===${buildFlavor}.a.Dev?${readOverrides}(n):null;return ${value}==null?`,
    );
  }

  const upsertMethod =
    '(?=[\\s\\S]{0,1500}?\\.upsertBrowserUsePIPContent\\(e,t,n,)';
  const upstreamUpsertGuard = new RegExp(
    `if\\(([A-Za-z_$][\\w$]*)!==\\\`darwin\\\`\\|\\|e\\.trim\\(\\)===\\\`\\\`\\|\\|t\\.trim\\(\\)===\\\`\\\`\\|\\|!n\\.startsWith\\(\\\`data:image/\\\`\\)\\)return!1;try\\{${upsertMethod}`,
  );
  const linuxUpsertGuard = new RegExp(
    `if\\(e\\.trim\\(\\)===\\\`\\\`\\|\\|t\\.trim\\(\\)===\\\`\\\`\\|\\|!n\\.startsWith\\(\\\`data:image/\\\`\\)\\)return!1;if\\(([A-Za-z_$][\\w$]*)!==\\\`darwin\\\`\\)return \\1===\\\`linux\\\`&&globalThis\\.__codexLinuxPipUpsert\\?\\.\\(\\{presentationId:e,threadId:t,dataUrl:n,backend:r\\}\\)===!0;try\\{${upsertMethod}`,
  );
  if (!linuxUpsertGuard.test(content)) {
    const matches = [...content.matchAll(new RegExp(upstreamUpsertGuard, 'g'))];
    if (matches.length !== 1) {
      throw new Error(`Expected one Browser PiP upsert guard, found ${matches.length}`);
    }
    content = content.replace(
      upstreamUpsertGuard,
      (_match, platform) =>
        `if(e.trim()===\`\`||t.trim()===\`\`||!n.startsWith(\`data:image/\`))return!1;if(${platform}!==\`darwin\`)return ${platform}===\`linux\`&&globalThis.__codexLinuxPipUpsert?.({presentationId:e,threadId:t,dataUrl:n,backend:r})===!0;try{`,
    );
  }

  const invalidateMethod =
    '(?=return\\(t\\?\\?[A-Za-z_$][\\w$]*\\(\\{electronAppPath:n,resourcesPath:i\\}\\)\\)\\.invalidateBrowserUsePIPContent\\(e\\))';
  const upstreamInvalidateGuard = new RegExp(
    `if\\(([A-Za-z_$][\\w$]*)!==\\\`darwin\\\`\\|\\|e\\.trim\\(\\)===\\\`\\\`\\)return!1;try\\{${invalidateMethod}`,
  );
  const linuxInvalidateGuard = new RegExp(
    `if\\(e\\.trim\\(\\)===\\\`\\\`\\)return!1;if\\(([A-Za-z_$][\\w$]*)!==\\\`darwin\\\`\\)return \\1===\\\`linux\\\`&&globalThis\\.__codexLinuxPipInvalidate\\?\\.\\(e\\)===!0;try\\{${invalidateMethod}`,
  );
  if (!linuxInvalidateGuard.test(content)) {
    const matches = [...content.matchAll(new RegExp(upstreamInvalidateGuard, 'g'))];
    if (matches.length !== 1) {
      throw new Error(
        `Expected one Browser PiP invalidation guard, found ${matches.length}`,
      );
    }
    content = content.replace(
      upstreamInvalidateGuard,
      (_match, platform) =>
        `if(e.trim()===\`\`)return!1;if(${platform}!==\`darwin\`)return ${platform}===\`linux\`&&globalThis.__codexLinuxPipInvalidate?.(e)===!0;try{`,
    );
  }

  const upstreamCursorDispatch =
    'e.windowManager.sendMessageToWebContents(i.owner,{type:`browser-sidebar-browser-use-cursor-state`,conversationId:t.conversationId,browserTabId:a,...c}),o=s||o';
  const previousLinuxCursorDispatch =
    'e.windowManager.sendMessageToWebContents(i.owner,{type:`browser-sidebar-browser-use-cursor-state`,conversationId:t.conversationId,browserTabId:a,...c}),globalThis.__codexLinuxPipCursor?.({conversationId:t.conversationId,browserTabId:a,...c}),o=s||o';
  const linuxCursorDispatch =
    'e.windowManager.sendMessageToWebContents(i.owner,{type:`browser-sidebar-browser-use-cursor-state`,conversationId:t.conversationId,browserTabId:a,...c}),globalThis.__codexLinuxPipCursor?.({conversationId:t.conversationId,browserTabId:a,viewport:e.getThreadStateForWindow(i,t.conversationId,a)?.emulatedViewportSize??null,...c}),o=s||o';
  if (!content.includes(linuxCursorDispatch)) {
    if (content.includes(previousLinuxCursorDispatch)) {
      content = content.replace(previousLinuxCursorDispatch, linuxCursorDispatch);
    } else if (content.includes(upstreamCursorDispatch)) {
      content = content.replace(upstreamCursorDispatch, linuxCursorDispatch);
    } else {
      throw new Error('Expected Browser Use cursor dispatch not found');
    }
  }

  const upstreamBrowserSync =
    'case`browser-sidebar-sync`:this.browserSidebarManager.sync(e,t.payload),this.onBrowserSidebarStateChanged();break;';
  const previousLinuxBrowserSync =
    'case`browser-sidebar-sync`:this.browserSidebarManager.sync(e,t.payload),globalThis.__codexLinuxPipPanelSync?.(t.payload),this.onBrowserSidebarStateChanged();break;';
  const linuxBrowserSync =
    'case`browser-sidebar-sync`:this.browserSidebarManager.sync(e,t.payload),globalThis.__codexLinuxPipPanelSync?.(t.payload,this.browserSidebarManager),this.onBrowserSidebarStateChanged();break;';
  if (!content.includes(linuxBrowserSync)) {
    if (content.includes(previousLinuxBrowserSync)) {
      content = content.replace(previousLinuxBrowserSync, linuxBrowserSync);
    } else if (content.includes(upstreamBrowserSync)) {
      content = content.replace(upstreamBrowserSync, linuxBrowserSync);
    } else {
      throw new Error('Expected Browser sidebar sync handler not found');
    }
  }

  const upstreamHiddenThreads =
    'if(t.type===`remote-hosted-pip-hidden-thread-ids-changed`){D?.(t.hiddenThreadIds);return}';
  const linuxHiddenThreads =
    'if(t.type===`remote-hosted-pip-hidden-thread-ids-changed`){D?.(t.hiddenThreadIds),globalThis.__codexLinuxPipHiddenThreads?.(t.hiddenThreadIds);return}';
  if (!content.includes(linuxHiddenThreads)) {
    const matchCount = content.split(upstreamHiddenThreads).length - 1;
    if (matchCount !== 1) {
      throw new Error(
        `Expected one hidden Browser PiP thread handler, found ${matchCount}`,
      );
    }
    content = content.replace(upstreamHiddenThreads, linuxHiddenThreads);
  }

  fs.writeFileSync(featureBundlePath, content);
}

function patchLinuxUi(appDir) {
  const bootstrapPath = path.join(appDir, '.vite', 'build', 'early-bootstrap.js');
  const runtimeSourcePath = path.join(__dirname, RUNTIME_NAME);
  const runtimeTargetPath = path.join(appDir, '.vite', 'build', RUNTIME_NAME);
  const preloadSourcePath = path.join(__dirname, PRELOAD_NAME);
  const preloadTargetPath = path.join(appDir, '.vite', 'build', PRELOAD_NAME);
  if (!fs.existsSync(bootstrapPath)) {
    throw new Error(`Expected upstream bootstrap not found: ${bootstrapPath}`);
  }
  if (!fs.existsSync(runtimeSourcePath)) {
    throw new Error(`Expected Linux UI runtime not found: ${runtimeSourcePath}`);
  }
  if (!fs.existsSync(preloadSourcePath)) {
    throw new Error(`Expected Linux UI preload not found: ${preloadSourcePath}`);
  }
  fs.copyFileSync(runtimeSourcePath, runtimeTargetPath);
  fs.copyFileSync(preloadSourcePath, preloadTargetPath);
  patchProductionFeatureOverrides(appDir);

  let content = fs.readFileSync(bootstrapPath, 'utf8');
  if (content.includes(MARKER)) {
    return;
  }

  const inject =
    `/* ${MARKER} */` +
    'require(require("node:path").join(__dirname,"linux-browser-pip.cjs"));';

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
