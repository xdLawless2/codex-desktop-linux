#!/usr/bin/env node

import { Buffer } from 'node:buffer';
import { createHash, webcrypto } from 'node:crypto';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  renameSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { createRequire } from 'node:module';
import { createConnection } from 'node:net';
import { homedir, tmpdir } from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath, pathToFileURL } from 'node:url';
import util from 'node:util';
import vm from 'node:vm';
import { parseModule } from 'meriyah';
import { parse as parseToml, stringify as stringifyToml } from 'smol-toml';

const SERVER_INFO = { name: 'node_repl', version: '1.0.0-linux' };
const PROTOCOL_VERSION = '2025-06-18';
const DEFAULT_TIMEOUT_MS = 30_000;
const CLIENT_REQUEST_TIMEOUT_MS = Number.parseInt(
  process.env.NODE_REPL_CLIENT_REQUEST_TIMEOUT_MS || '10000',
  10,
);
const ELICITATION_TIMEOUT_MS = Number.parseInt(
  process.env.NODE_REPL_ELICITATION_TIMEOUT_MS || '300000',
  10,
);
const NATIVE_PIPE_CONNECT_TIMEOUT_MS = Number.parseInt(
  process.env.NODE_REPL_NATIVE_PIPE_CONNECT_TIMEOUT_MS || '1000',
  10,
);
const TRUSTED_BROWSER_CLIENT_HASHES = new Set(
  (process.env.NODE_REPL_TRUSTED_BROWSER_CLIENT_SHA256S || '')
    .split(',')
    .map((value) => value.trim().toLowerCase())
    .filter(Boolean),
);
const DEFAULT_REQUEST_META = (() => {
  const value = process.env.NODE_REPL_REQUEST_META?.trim();
  if (!value) {
    return {};
  }
  const parsed = JSON.parse(value);
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('NODE_REPL_REQUEST_META must be a JSON object');
  }
  return parsed;
})();
const promptsPath = process.env.NODE_REPL_PROMPTS_PATH || new URL(
  './official-prompts.json',
  import.meta.url,
);
const PROMPTS = JSON.parse(readFileSync(promptsPath, 'utf8'));
const USE_CASES = Object.entries(process.env)
  .filter(([name, value]) => name.startsWith('NODE_REPL_INSTRUCTIONS_USE_CASE_') && value)
  .sort(([left], [right]) => left.localeCompare(right))
  .map(([, value]) => value);
const LINUX_BROWSER_INSTRUCTIONS =
  'In-app Browser progress is presented through the floating Browser PiP by default. Enable the Browser visibility capability (the full right-hand Browser Pane) only when the user explicitly asks to see the full browser view. Reuse an existing IAB tab for the same site so its persistent profile, cookies, storage, and cache remain effective. Avoid rapid navigation retries and reload loops. Submit at most one visual browser action per js tool call so the PiP receives a frame for every click, drag, keypress, scroll, type, or navigation.';
const SERVER_INSTRUCTIONS = `${PROMPTS.serverInstructions}${
  USE_CASES.length === 0
    ? ''
    : `\n\nUse Cases:\n${USE_CASES.map((value) => `- ${value}`).join('\n')}`
}\n\nLinux Browser presentation:\n${LINUX_BROWSER_INSTRUCTIONS}`;

let execution;
let nextClientRequestId = 1;
const moduleResolvers = new Map();
const pendingClientRequests = new Map();

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function textContent(text) {
  return { type: 'text', text };
}

function requestClient(method, params, timeoutMs = CLIENT_REQUEST_TIMEOUT_MS) {
  const id = `node-repl-${nextClientRequestId++}`;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pendingClientRequests.delete(id);
      reject(new Error(`MCP client request timed out: ${method}`));
    }, timeoutMs);
    pendingClientRequests.set(id, {
      reject(error) {
        clearTimeout(timer);
        reject(error);
      },
      resolve(value) {
        clearTimeout(timer);
        resolve(value);
      },
    });
    send({ jsonrpc: '2.0', id, method, params });
  });
}

function resolveCodexTomlPath(value, { writable = false } = {}) {
  if (typeof value !== 'string' || value.length === 0) {
    throw new Error('nodeRepl TOML path must be a non-empty string');
  }
  if (writable && value.toLowerCase() === 'config.toml') {
    throw new Error(
      'nodeRepl.config.writeToml does not allow writing ~/.codex/config.toml; use nodeRepl.config.writeValue or nodeRepl.config.batchWrite',
    );
  }
  const codexHome = path.resolve(process.env.CODEX_HOME || path.join(homedir(), '.codex'));
  const resolved = path.resolve(codexHome, value);
  if (resolved !== codexHome && !resolved.startsWith(`${codexHome}${path.sep}`)) {
    throw new Error(`nodeRepl TOML path escapes CODEX_HOME: ${value}`);
  }
  return resolved;
}

function readCodexToml(value) {
  const resolved = resolveCodexTomlPath(value);
  return existsSync(resolved) ? parseToml(readFileSync(resolved, 'utf8')) : {};
}

function writeCodexToml(pathValue, value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('nodeRepl TOML value must be a plain object');
  }
  const resolved = resolveCodexTomlPath(pathValue, { writable: true });
  mkdirSync(path.dirname(resolved), { recursive: true });
  const temporary = `${resolved}.${process.pid}.tmp`;
  writeFileSync(temporary, stringifyToml(structuredClone(value)), { encoding: 'utf8', mode: 0o600 });
  renameSync(temporary, resolved);
}

function inspect(value) {
  return util.inspect(value, {
    colors: false,
    compact: false,
    depth: 8,
    maxArrayLength: 200,
    maxStringLength: 100_000,
  });
}

function persistTopLevelBindings(code) {
  const program = parseModule(code, { next: true, ranges: true });
  const replacements = [];
  for (const statement of program.body) {
    if (
      statement.type !== 'VariableDeclaration' ||
      !statement.declarations.every((declaration) => declaration.id.type === 'Identifier')
    ) {
      continue;
    }
    const replacement = statement.declarations
      .map((declaration) => {
        const name = JSON.stringify(declaration.id.name);
        if (declaration.init == null) {
          return `if (!(${name} in globalThis)) globalThis[${name}] = undefined`;
        }
        return `globalThis[${name}] = (${code.slice(declaration.init.start, declaration.init.end)})`;
      })
      .join(';');
    replacements.push({ end: statement.end, replacement: `${replacement};`, start: statement.start });
  }
  for (const { end, replacement, start } of replacements.reverse()) {
    code = `${code.slice(0, start)}${replacement}${code.slice(end)}`;
  }
  return code;
}

async function withTimeout(promise, timeoutMs, message) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(message)), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

function normalizeImport(specifier) {
  if (specifier.startsWith('file:')) {
    return { path: fileURLToPath(specifier), url: specifier };
  }
  if (path.isAbsolute(specifier)) {
    return { path: specifier, url: pathToFileURL(specifier).href };
  }
  throw new Error(`node_repl only permits absolute trusted plugin imports: ${specifier}`);
}

function importTrustedPlugin(specifier) {
  if (specifier.startsWith('node:')) {
    return import(specifier);
  }
  if (!specifier.startsWith('file:') && !path.isAbsolute(specifier)) {
    for (const resolve of moduleResolvers.values()) {
      try {
        return import(pathToFileURL(resolve(specifier)).href);
      } catch (error) {
        if (error?.code !== 'MODULE_NOT_FOUND' && error?.code !== 'ERR_PACKAGE_PATH_NOT_EXPORTED') {
          throw error;
        }
      }
    }
    throw new Error(`node_repl could not resolve module: ${specifier}`);
  }
  const resolved = normalizeImport(specifier);
  const registeredModule = [...moduleResolvers.keys()].some(
    (directory) => resolved.path === directory || resolved.path.startsWith(`${directory}${path.sep}`),
  );
  const trustedBrowserClient =
    path.basename(resolved.path) === 'browser-client.mjs' &&
    TRUSTED_BROWSER_CLIENT_HASHES.has(
      createHash('sha256').update(readFileSync(resolved.path)).digest('hex'),
    );
  if (!trustedBrowserClient && !registeredModule) {
    throw new Error(`node_repl rejected untrusted import: ${resolved.path}`);
  }
  return import(resolved.url);
}

function addNodeModuleDirectory(directory) {
  if (typeof directory !== 'string' || !path.isAbsolute(directory)) {
    throw new Error('js_add_node_module_dir requires an absolute directory path');
  }
  const resolved = realpathSync(directory);
  if (!statSync(resolved).isDirectory()) {
    throw new Error(`Node module path is not a directory: ${resolved}`);
  }
  if (moduleResolvers.has(resolved)) {
    return false;
  }
  moduleResolvers.set(resolved, createRequire(path.join(resolved, '__node_repl__.cjs')).resolve);
  return true;
}

for (const directory of (process.env.NODE_REPL_NODE_MODULE_DIRS || '')
  .split(path.delimiter)
  .map((value) => value.trim())
  .filter(Boolean)) {
  addNodeModuleDirectory(directory);
}

function imageContent(value) {
  if (typeof value === 'string' && value.startsWith('data:')) {
    const match = /^data:([^;,]+);base64,(.+)$/s.exec(value);
    if (!match) {
      throw new Error('nodeRepl.emitImage expects a base64 data URL');
    }
    return { type: 'image', mimeType: match[1], data: match[2] };
  }
  if (value && typeof value === 'object' && typeof value.image_url === 'string') {
    return imageContent(value.image_url);
  }
  if (Buffer.isBuffer(value) || value instanceof Uint8Array) {
    return { type: 'image', mimeType: 'image/png', data: Buffer.from(value).toString('base64') };
  }
  throw new Error('nodeRepl.emitImage received an unsupported value');
}

function connectNativePipe(socketPath) {
  if (typeof socketPath !== 'string' || socketPath.length === 0) {
    throw new Error('nodeRepl.nativePipe.createConnection expects a socket path');
  }
  return new Promise((resolve, reject) => {
    const socket = createConnection(socketPath);
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`nodeRepl native pipe connection timed out: ${socketPath}`));
    }, NATIVE_PIPE_CONNECT_TIMEOUT_MS);
    const onError = (error) => {
      clearTimeout(timer);
      socket.destroy();
      reject(error);
    };
    socket.once('error', onError);
    socket.once('connect', () => {
      clearTimeout(timer);
      socket.off('error', onError);
      resolve(socket);
    });
  });
}

function createExecution() {
  const state = {
    afterHooks: [],
    content: [],
    requestMeta: {},
    responseMeta: {},
  };

  const nodeRepl = {
    addAfterSubmittedCodeHook(options) {
      if (
        !options ||
        typeof options.run !== 'function' ||
        !Number.isInteger(options.timeoutMs) ||
        options.timeoutMs <= 0
      ) {
        throw new Error(
          'nodeRepl.addAfterSubmittedCodeHook expected { run: function, timeoutMs: positive integer }',
        );
      }
      state.afterHooks.push(options);
    },
    config: Object.freeze({
      async batchWrite(request) {
        return requestClient('config/batchWrite', request);
      },
      async read() {
        return { config: readCodexToml('config.toml') };
      },
      async readRequirements() {
        return { requirements: null };
      },
      async readToml(pathValue) {
        return readCodexToml(await pathValue);
      },
      async writeToml(pathValue, value) {
        writeCodexToml(await pathValue, await value);
      },
      async writeValue(request) {
        return requestClient('config/value/write', request);
      },
    }),
    cwd: process.cwd(),
    createElicitation(request) {
      if (!request || typeof request !== 'object' || Array.isArray(request)) {
        throw new Error('nodeRepl.createElicitation expected a request object');
      }
      // Browser origin-access grants would otherwise be auto-rejected by the
      // "never ask" approval policy, breaking every IAB navigation.
      if (
        request.meta?.connector_id === 'browser-use' &&
        request.meta?.tool_name === 'access_browser_origin'
      ) {
        return Promise.resolve({ action: 'accept', content: {} });
      }
      return requestClient('elicitation/create', request, ELICITATION_TIMEOUT_MS);
    },
    emitImage(value) {
      state.content.push(imageContent(value));
    },
    env: Object.freeze({ ...process.env }),
    homeDir: homedir(),
    nativePipe: Object.freeze({ createConnection: connectNativePipe }),
    get requestMeta() {
      return state.requestMeta;
    },
    setResponseMeta(value) {
      if (!value || typeof value !== 'object' || Array.isArray(value)) {
        throw new Error('nodeRepl.setResponseMeta expected an object');
      }
      state.responseMeta = { ...state.responseMeta, ...structuredClone(value) };
    },
    tmpDir: tmpdir(),
    write(value) {
      state.content.push(textContent(typeof value === 'string' ? value : inspect(value)));
      return value;
    },
  };
  globalThis.nodeRepl = nodeRepl;

  const consoleProxy = {};
  for (const method of ['debug', 'error', 'info', 'log', 'warn']) {
    consoleProxy[method] = (...values) => {
      state.content.push(textContent(values.map(inspect).join(' ')));
    };
  }

  const sandbox = {
    AbortController,
    AbortSignal,
    Blob,
    Buffer,
    console: Object.freeze(consoleProxy),
    crypto: webcrypto,
    fetch,
    FormData,
    Headers,
    nodeRepl: Object.freeze(nodeRepl),
    performance,
    queueMicrotask,
    Request,
    Response,
    setImmediate,
    setInterval,
    setTimeout,
    structuredClone,
    TextDecoder,
    TextEncoder,
    URL,
    URLSearchParams,
    clearImmediate,
    clearInterval,
    clearTimeout,
  };
  const context = vm.createContext(sandbox, {
    codeGeneration: { strings: true, wasm: false },
    name: 'codex-node-repl',
  });

  return { context, state };
}

function resetExecution() {
  execution = createExecution();
}

async function evaluate(code, timeoutMs, requestMeta) {
  const { context, state } = execution;
  state.content = [];
  state.requestMeta = { ...DEFAULT_REQUEST_META, ...(requestMeta ?? {}) };
  state.responseMeta = {};

  const options = {
    filename: 'node_repl_input.mjs',
    importModuleDynamically: importTrustedPlugin,
    timeout: timeoutMs,
  };

  let promise;
  try {
    promise = vm.runInContext(`(async () => (${code}\n))()`, context, options);
  } catch (error) {
    if (error?.name !== 'SyntaxError') {
      throw error;
    }
    promise = vm.runInContext(
      `(async () => {\n${persistTopLevelBindings(code)}\n})()`,
      context,
      options,
    );
  }

  const result = await withTimeout(
    promise,
    timeoutMs,
    `JavaScript execution timed out after ${timeoutMs}ms`,
  );

  if (result !== undefined && state.content.length === 0) {
    state.content.push(textContent(inspect(result)));
  }

  for (const hook of state.afterHooks) {
    await withTimeout(
      hook.run(),
      hook.timeoutMs,
      `After-code hook timed out after ${hook.timeoutMs}ms`,
    );
  }

  return {
    content: state.content.length > 0 ? state.content : [textContent('JavaScript completed.')],
    responseMeta: state.responseMeta,
  };
}

function tools() {
  return [
    {
      name: 'js',
      description: PROMPTS.jsDescription,
      inputSchema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          code: { type: 'string', description: PROMPTS.jsCodeDescription },
          timeout_ms: {
            type: 'integer',
            minimum: 1,
            maximum: 600_000,
            description: PROMPTS.jsTimeoutDescription,
          },
          title: {
            type: 'string',
            maxLength: 80,
            description: PROMPTS.jsTitleDescription,
          },
        },
        required: ['code'],
      },
    },
    {
      name: 'js_reset',
      description: PROMPTS.resetDescription,
      inputSchema: { type: 'object', additionalProperties: false, properties: {} },
    },
    {
      name: 'js_add_node_module_dir',
      description: PROMPTS.addNodeModuleDirDescription,
      inputSchema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          path: {
            type: 'string',
            minLength: 1,
            description: PROMPTS.nodeModuleDirPathDescription,
          },
        },
        required: ['path'],
      },
    },
  ];
}

async function handleRequest(message) {
  const { id, method, params = {} } = message;

  if (method === 'initialize') {
    return {
      jsonrpc: '2.0',
      id,
      result: {
        protocolVersion: params.protocolVersion ?? PROTOCOL_VERSION,
        capabilities: { tools: { listChanged: false } },
        serverInfo: SERVER_INFO,
        instructions: SERVER_INSTRUCTIONS,
      },
    };
  }
  if (method === 'ping') {
    return { jsonrpc: '2.0', id, result: {} };
  }
  if (method === 'tools/list') {
    return { jsonrpc: '2.0', id, result: { tools: tools() } };
  }
  if (method !== 'tools/call') {
    return {
      jsonrpc: '2.0',
      id,
      error: { code: -32601, message: `Method not found: ${method}` },
    };
  }

  const name = params.name;
  const args = params.arguments ?? {};
  if (name === 'js_reset') {
    resetExecution();
    return { jsonrpc: '2.0', id, result: { content: [textContent('JavaScript context reset.')] } };
  }
  if (name === 'js_add_node_module_dir') {
    const added = addNodeModuleDirectory(args.path);
    return {
      jsonrpc: '2.0',
      id,
      result: { content: [textContent(String(added))] },
    };
  }
  if (name !== 'js') {
    return {
      jsonrpc: '2.0',
      id,
      error: { code: -32602, message: `Unknown tool: ${name}` },
    };
  }

  try {
    const timeoutMs = Number.isInteger(args.timeout_ms) ? args.timeout_ms : DEFAULT_TIMEOUT_MS;
    if (typeof args.code !== 'string' || args.code.length === 0) {
      throw new Error('js requires non-empty code');
    }
    const result = await evaluate(args.code, timeoutMs, params._meta);
    return {
      jsonrpc: '2.0',
      id,
      result: {
        content: result.content,
        isError: false,
        ...(Object.keys(result.responseMeta).length > 0 ? { _meta: result.responseMeta } : {}),
      },
    };
  } catch (error) {
    return {
      jsonrpc: '2.0',
      id,
      result: {
        content: [textContent(error instanceof Error ? error.stack ?? error.message : String(error))],
        isError: true,
      },
    };
  }
}

resetExecution();

const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
let requestQueue = Promise.resolve();
input.on('line', (line) => {
  if (!line.trim()) {
    return;
  }
  let message;
  try {
    message = JSON.parse(line);
  } catch (error) {
    send({
      jsonrpc: '2.0',
      id: null,
      error: {
        code: -32700,
        message: error instanceof Error ? error.message : String(error),
      },
    });
    return;
  }

  if (message.method === undefined && pendingClientRequests.has(message.id)) {
    const pending = pendingClientRequests.get(message.id);
    pendingClientRequests.delete(message.id);
    if (message.error) {
      pending.reject(new Error(message.error.message ?? 'MCP client request failed'));
    } else {
      pending.resolve(message.result);
    }
    return;
  }

  requestQueue = requestQueue.then(async () => {
    try {
      if (message.id === undefined) {
        return;
      }
      send(await handleRequest(message));
    } catch (error) {
      send({
        jsonrpc: '2.0',
        id: null,
        error: {
          code: -32700,
          message: error instanceof Error ? error.message : String(error),
        },
      });
    }
  });
});
