/**
 * RecurAgent M6 PTC (Programmatic Tool Calling) SDK injection script.
 *
 * The model emits a JS program; the host (pi) spawns this file in a separate
 * node process. Inside that process the program orchestrates multi-step tool
 * calls through the `sdk` object while every tool is executed host-side.
 *
 * Transport (auto-detected):
 *   1. IPC:     `process.send` / `process.on('message')` when forked by pi.
 *   2. Stdio:   JSON-lines over stdin/stdout when spawned without an IPC channel.
 *
 * Protocol (one JSON message per IPC message / per line):
 *   request  SDK -> host:  { id, tool, args }
 *   response host -> SDK:  { id, ok, result }   or   { id, ok: false, error }
 *   terminal SDK -> host:  { id: "result", ok: true, result }
 *                        | { id: "result", ok: false, error }
 *
 * Rules:
 *   - Host-injected globals are validated before use: each must be a usable
 *     identifier, not a reserved word, and unique (`validateBindingNames`).
 *   - Every tool call times out after 30s (`PTC_TOOL_TIMEOUT_MS` overrides;
 *     the host sets it to the enclosing run budget) and rejects its Promise.
 *   - A failed tool rejects only the awaiting Promise; the program keeps running.
 *   - Debug output from user code goes to stderr so stdout stays protocol-clean.
 *   - Raw `fs` / `child_process` are never exposed to the program; it may only
 *     reach tools the host has whitelisted.
 *
 * Entry point:
 *   node assets/ptc_sdk.js [--code-file <path>] [codePath]
 *   or set PTC_CODE_FILE / PTC_CODE. The code is executed with
 *   `new AsyncFunction(...INJECTED_BINDINGS, code)` and its return value is
 *   serialized into the terminal message.
 */
'use strict';

const fs = require('fs');

/**
 * Host-injected binding names (the SDK surface handed to the program).
 *
 * Validated before injection the same way DeepSeek Harness `bindings.ts` does:
 * a name must be a usable JS identifier, must not be a reserved word, and must
 * not collide with another injected global. This keeps a malformed host
 * invocation from silently shadowing a global or failing with a syntax error.
 */
const INJECTED_BINDINGS = ['sdk', 'console'];

const IDENTIFIER = /^[A-Za-z_$][A-Za-z0-9_$]*$/;
const RESERVED_WORDS = new Set([
  'await', 'break', 'case', 'catch', 'class', 'const', 'continue', 'debugger',
  'default', 'delete', 'do', 'else', 'enum', 'export', 'extends', 'false',
  'finally', 'for', 'function', 'if', 'implements', 'import', 'in',
  'instanceof', 'interface', 'let', 'new', 'null', 'package', 'private',
  'protected', 'public', 'return', 'static', 'super', 'switch', 'this',
  'throw', 'true', 'try', 'typeof', 'var', 'void', 'while', 'with', 'yield',
]);

/**
 * Validate the names the host wants injected as program globals.
 * @param {string[]} names - Candidate binding names.
 * @returns {string[]} The same names when every one is usable.
 * @throws {Error} When a name is not injectable.
 */
function validateBindingNames(names) {
  const seen = new Set();
  for (const name of names) {
    if (typeof name !== 'string' || !IDENTIFIER.test(name) || RESERVED_WORDS.has(name)) {
      throw new Error(`binding global ${JSON.stringify(name)} is not a usable identifier`);
    }
    if (seen.has(name)) {
      throw new Error(`duplicate binding global ${JSON.stringify(name)}`);
    }
    seen.add(name);
  }
  return names;
}

const TOOL_TIMEOUT_MS = Number(process.env.PTC_TOOL_TIMEOUT_MS) > 0
  ? Number(process.env.PTC_TOOL_TIMEOUT_MS)
  : 30_000;
const RESULT_ID = 'result';

let nextId = 1;
const pending = new Map();

const ipcMode = typeof process.send === 'function';
let transportReady = false;

/* ------------------------------------------------------------------ */
/* Transport                                                           */
/* ------------------------------------------------------------------ */

function send(message) {
  if (ipcMode) {
    process.send(message);
    return;
  }
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function finish(message) {
  send(message);
  // Give the queued write a tick to flush before releasing the event loop.
  setImmediate(() => {
    if (ipcMode) {
      if (typeof process.disconnect === 'function') {
        try {
          process.disconnect();
        } catch {
          // Channel already gone; nothing to do.
        }
      }
      return;
    }
    process.stdin.pause();
  });
}

function rejectAll(reason) {
  for (const [, entry] of pending) {
    clearTimeout(entry.timer);
    entry.reject(reason);
  }
  pending.clear();
}

function errorText(error) {
  if (error instanceof Error) return error.message;
  if (typeof error === 'string') return error;
  if (error && typeof error === 'object') {
    if (typeof error.message === 'string') return error.message;
    try {
      return JSON.stringify(error);
    } catch {
      return String(error);
    }
  }
  return String(error);
}

function handleMessage(message) {
  if (!message || typeof message !== 'object') return;
  const entry = pending.get(message.id);
  if (!entry) return;
  pending.delete(message.id);
  clearTimeout(entry.timer);
  if (message.ok) {
    entry.resolve(message.result);
  } else {
    entry.reject(new Error(errorText(message.error)));
  }
}

function attachStdinTransport() {
  let buffer = '';
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (!line) continue;
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        process.stderr.write(`[ptc] ignoring malformed host line: ${line}\n`);
        continue;
      }
      handleMessage(message);
    }
  });
  process.stdin.on('end', () => rejectAll(new Error('host closed stdin')));
  process.stdin.resume();
}

function ensureTransport() {
  if (transportReady) return;
  transportReady = true;
  if (ipcMode) {
    process.on('message', handleMessage);
    process.on('disconnect', () => rejectAll(new Error('host disconnected')));
  } else {
    attachStdinTransport();
  }
}

/* ------------------------------------------------------------------ */
/* Tool calls                                                          */
/* ------------------------------------------------------------------ */

function call(tool, args = {}) {
  ensureTransport();
  return new Promise((resolve, reject) => {
    const id = nextId++;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`tool "${tool}" timed out after ${TOOL_TIMEOUT_MS}ms`));
    }, TOOL_TIMEOUT_MS);
    pending.set(id, { resolve, reject, timer });
    try {
      send({ id, tool, args });
    } catch (err) {
      clearTimeout(timer);
      pending.delete(id);
      reject(new Error(`failed to send tool "${tool}": ${errorText(err)}`));
    }
  });
}

function requireString(name, value) {
  if (typeof value !== 'string' || value.length === 0) {
    throw new Error(`${name} must be a non-empty string`);
  }
}

async function read(path) {
  requireString('path', path);
  return call('read', { path });
}

async function grep(pattern, path) {
  requireString('pattern', pattern);
  if (path !== undefined && path !== null) requireString('path', path);
  return call('grep', path === undefined || path === null ? { pattern } : { pattern, path });
}

async function ls(path) {
  requireString('path', path);
  return call('ls', { path });
}

async function find(pattern, path) {
  requireString('pattern', pattern);
  if (path !== undefined && path !== null) requireString('path', path);
  return call('find', path === undefined || path === null ? { pattern } : { pattern, path });
}

// The bridge is read-only by contract: the host rejects any tool outside
// BRIDGE_WHITELIST (read/grep/find/ls). `write`, `edit` and `bash` are
// deliberately NOT exported so the SDK surface matches what the host will
// actually allow — no misleading, always-denied bindings.
const sdk = { read, grep, find, ls, call };

/* ------------------------------------------------------------------ */
/* Serialization                                                       */
/* ------------------------------------------------------------------ */

function serializeError(error) {
  if (error instanceof Error) {
    return { name: error.name, message: error.message, stack: error.stack };
  }
  return { name: 'Error', message: errorText(error) };
}

function serializeValue(value) {
  if (value === undefined || typeof value === 'function') return null;
  if (typeof value === 'bigint') return value.toString();
  if (typeof value === 'symbol') return value.toString();
  if (value instanceof Error) return serializeError(value);

  const seen = new WeakSet();
  try {
    const json = JSON.stringify(value, (_key, current) => {
      if (typeof current === 'bigint') return current.toString();
      if (typeof current === 'function') return `[Function ${current.name || 'anonymous'}]`;
      if (typeof current === 'symbol') return current.toString();
      if (current && typeof current === 'object') {
        if (seen.has(current)) return '[Circular]';
        seen.add(current);
      }
      return current;
    });
    return json === undefined ? null : JSON.parse(json);
  } catch {
    return String(value);
  }
}

/* ------------------------------------------------------------------ */
/* Execution entry                                                     */
/* ------------------------------------------------------------------ */

function loadCode(argv) {
  if (typeof argv === 'string' && argv.length > 0) return argv;

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if ((arg === '--code-file' || arg === '-f') && argv[i + 1]) {
      return fs.readFileSync(argv[i + 1], 'utf8');
    }
    if (!arg.startsWith('-')) return fs.readFileSync(arg, 'utf8');
  }
  if (process.env.PTC_CODE_FILE) return fs.readFileSync(process.env.PTC_CODE_FILE, 'utf8');
  if (process.env.PTC_CODE) return process.env.PTC_CODE;
  throw new Error('no program code: pass --code-file <path> or set PTC_CODE_FILE / PTC_CODE');
}

const ptcConsole = {};
for (const level of ['log', 'info', 'debug', 'warn', 'error']) {
  ptcConsole[level] = (...parts) => {
    process.stderr.write(`[ptc:${level}] ${parts.map(String).join(' ')}\n`);
  };
}

async function main(codeOrArgv) {
  try {
    const code = typeof codeOrArgv === 'string'
      ? codeOrArgv
      : loadCode(Array.isArray(codeOrArgv) ? codeOrArgv : process.argv.slice(2));

    const AsyncFunction = Object.getPrototypeOf(async function noop() {}).constructor;
    // Reject an unusable host injection list before compiling anything.
    validateBindingNames(INJECTED_BINDINGS);
    const program = new AsyncFunction(...INJECTED_BINDINGS, `"use strict";\n${code}`);
    const result = await program(sdk, ptcConsole);
    finish({ id: RESULT_ID, ok: true, result: serializeValue(result) });
    return result;
  } catch (err) {
    process.exitCode = 1;
    finish({ id: RESULT_ID, ok: false, error: serializeError(err) });
    return undefined;
  }
}

module.exports = {
  sdk,
  call,
  read,
  grep,
  find,
  ls,
  main,
  loadCode,
  serializeValue,
  serializeError,
  validateBindingNames,
  INJECTED_BINDINGS,
  TOOL_TIMEOUT_MS,
};

if (require.main === module) {
  main();
}
