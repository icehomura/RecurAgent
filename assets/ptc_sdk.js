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
 *   - Output discipline: `console.*` writes to stderr, and any stray
 *     `process.stdout.write` from program code (or a dependency) is redirected
 *     to stderr too, so it can never corrupt the protocol channel. Only the
 *     `return`ed value is delivered to the model.
 *   - `process.exit` is refused with an actionable error instead of tearing the
 *     channel down mid-run; an unexpected exit, uncaught exception, or
 *     unhandled rejection is reported as a structured failure rather than a
 *     bare EOF.
 *   - Error stacks are rebased onto the program's own line numbers, so a frame
 *     like `ptc-program:12:5` names the line in the `code` the model wrote.
 *   - `fs` / `child_process` are never injected as bindings. The child is
 *     confined by Node's permission model when the host enables it (default):
 *     reads are limited to the program's scratch dir plus the session workspace
 *     roots, and writes, `child_process`, and reads outside those roots are
 *     denied by the runtime.
 *
 * Tool call shapes (each helper accepts a positional string OR an options
 * object; the object form forwards every key to the host tool):
 *   sdk.read(path)                       sdk.read({ path, offset, limit, hashline, encoding })
 *   sdk.grep(pattern, pathOrOptions?)    sdk.grep({ pattern, path, glob, ignoreCase, literal, context, limit, hashline })
 *   sdk.find(pattern, pathOrOptions?)    sdk.find({ pattern, path, limit })
 *   sdk.ls(pathOrOptions?)               sdk.ls({ path, limit })
 *   sdk.call(tool, args)                 full argument set for any whitelisted tool
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
const PROGRAM_FILENAME = 'ptc-program';

let nextId = 1;
const pending = new Map();

const ipcMode = typeof process.send === 'function';
let transportReady = false;

/** Whether a terminal message has been produced; guards against duplicates. */
let settled = false;
/** Lines to subtract from `<anonymous>:N:` frames to reach program line numbers. */
let sourceLineOffset = 0;

/* ------------------------------------------------------------------ */
/* Output discipline                                                   */
/* ------------------------------------------------------------------ */

// Captured before any guard is installed: the protocol always writes through
// this handle, so redirecting `process.stdout.write` below cannot break it.
const realStdoutWrite = process.stdout.write.bind(process.stdout);

/**
 * Redirect program/dependency stdout to stderr.
 *
 * The stdout channel carries the JSON-lines protocol; a single stray line (a
 * `console.log` fallback, a dependency banner, a native addon write) makes the
 * host see non-protocol output. Routing those bytes to stderr keeps the
 * channel clean and still surfaces the text in the host's stderr drain.
 */
function installStdoutGuard() {
  if (ipcMode) return; // IPC mode: the protocol does not use stdout.
  process.stdout.write = function guardedWrite(chunk, encoding, callback) {
    let text;
    if (typeof chunk === 'string') {
      text = chunk;
    } else if (chunk instanceof Uint8Array) {
      text = Buffer.from(chunk).toString('utf8');
    } else {
      text = String(chunk);
    }
    process.stderr.write(`[ptc:stdout] ${text}`);
    const done = typeof encoding === 'function' ? encoding : callback;
    if (typeof done === 'function') queueMicrotask(done);
    return true;
  };
}

/* ------------------------------------------------------------------ */
/* Transport                                                           */
/* ------------------------------------------------------------------ */

function send(message) {
  if (ipcMode) {
    process.send(message);
    return;
  }
  realStdoutWrite(`${JSON.stringify(message)}\n`);
}

/**
 * Emit the terminal message synchronously, bypassing buffered streams.
 *
 * Only used from the `process.exit` backstop, where the event loop is already
 * unwinding and an async write would not flush.
 */
function writeTerminalNow(message) {
  if (settled) return;
  settled = true;
  if (ipcMode) {
    try {
      process.send(message);
    } catch {
      // Channel already gone; nothing to do.
    }
    return;
  }
  try {
    fs.writeSync(1, `${JSON.stringify(message)}\n`);
  } catch {
    // stdout already closed; the host will surface PTC_EOF.
  }
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

/** Send at most one terminal message; later calls are no-ops. */
function settle(message) {
  if (settled) return;
  settled = true;
  finish(message);
}

/** Send at most one terminal failure; later calls are no-ops. */
function settleError(error) {
  settle({
    id: RESULT_ID,
    ok: false,
    error: serializeError(error instanceof Error ? error : new Error(errorText(error))),
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

/**
 * Normalize a helper's first argument into the host tool's argument object.
 *
 * Accepts the positional shorthand (a non-empty string) or a full options
 * object, and validates the one required key either way. The object form is
 * forwarded verbatim, so every option the host tool understands (offset,
 * limit, hashline, encoding, glob, context, ...) takes effect instead of being
 * silently dropped.
 *
 * @param {unknown} value - Positional string or options object.
 * @param {string} key - Required key (`path` / `pattern`).
 * @param {string} form - Helper name, for error messages.
 * @returns {Record<string, unknown>} Arguments for the host tool.
 */
function normalizeArgs(value, key, form) {
  if (typeof value === 'string') {
    if (value.length === 0) {
      throw new Error(`sdk.${form}: \`${key}\` must be a non-empty string`);
    }
    return { [key]: value };
  }
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    const args = { ...value };
    const required = args[key];
    if (typeof required !== 'string' || required.length === 0) {
      throw new Error(
        `sdk.${form}: an options object needs a non-empty \`${key}\` string, got ` +
        `${JSON.stringify(required)}`
      );
    }
    return args;
  }
  throw new Error(
    `sdk.${form}: expected a string or an options object with \`${key}\`, got ` +
    `${value === null ? 'null' : typeof value}. ` +
    `Use sdk.${form}("...") or sdk.${form}({ ${key}: "..." }).`
  );
}

/**
 * Merge a second scope argument (path string or options object) into `args`.
 * @param {Record<string, unknown>} args - Already-normalized first argument.
 * @param {unknown} scope - Optional second argument.
 * @param {string} form - Helper name, for error messages.
 * @returns {Record<string, unknown>} Arguments for the host tool.
 */
function mergeScope(args, scope, form) {
  if (scope === undefined || scope === null) return args;
  if (typeof scope === 'string') {
    if (scope.length === 0) {
      throw new Error(`sdk.${form}: second argument must be a non-empty path string`);
    }
    return { ...args, path: scope };
  }
  if (typeof scope === 'object' && !Array.isArray(scope)) {
    return { ...args, ...scope };
  }
  throw new Error(
    `sdk.${form}: second argument must be a path string or an options object, got ${typeof scope}`
  );
}

async function read(pathOrOptions) {
  return call('read', normalizeArgs(pathOrOptions, 'path', 'read'));
}

async function grep(patternOrOptions, pathOrOptions) {
  const args = normalizeArgs(patternOrOptions, 'pattern', 'grep');
  return call('grep', mergeScope(args, pathOrOptions, 'grep'));
}

async function ls(pathOrOptions) {
  // `ls` treats the path as optional (defaults to the working directory), so a
  // bare call, or an options object that only carries `limit`, is valid.
  if (pathOrOptions === undefined || pathOrOptions === null) {
    return call('ls', {});
  }
  if (typeof pathOrOptions === 'object' && !Array.isArray(pathOrOptions)) {
    const args = { ...pathOrOptions };
    if (args.path !== undefined && (typeof args.path !== 'string' || args.path.length === 0)) {
      throw new Error(
        `sdk.ls: \`path\` must be a non-empty string when present, got ${JSON.stringify(args.path)}`
      );
    }
    return call('ls', args);
  }
  return call('ls', normalizeArgs(pathOrOptions, 'path', 'ls'));
}

async function find(patternOrOptions, pathOrOptions) {
  const args = normalizeArgs(patternOrOptions, 'pattern', 'find');
  return call('find', mergeScope(args, pathOrOptions, 'find'));
}

// The bridge is read-only by contract: the host rejects any tool outside
// BRIDGE_WHITELIST (read/grep/find/ls). `write`, `edit` and `bash` are
// deliberately NOT exported so the SDK surface matches what the host will
// actually allow — no misleading, always-denied bindings.
const sdk = { read, grep, find, ls, call };

/* ------------------------------------------------------------------ */
/* Serialization                                                       */
/* ------------------------------------------------------------------ */

/**
 * Rebase `<anonymous>:N:` frames onto the program's own line numbers.
 *
 * The program is compiled as an `AsyncFunction` body, so node reports lines
 * offset by the generated wrapper. The offset is measured once at startup
 * (`computeSourceLineOffset`) rather than assumed, so it stays correct if the
 * binding list or node's wrapper shape changes.
 *
 * @param {string|undefined} stack - Raw `error.stack`.
 * @returns {string|undefined} Stack with `<anonymous>` frames rebased.
 */
function mapStackToUserCode(stack) {
  if (!stack || sourceLineOffset <= 0) return stack;
  return stack.replace(/<anonymous>:(\d+):(\d+)/g, (match, line, column) => {
    const adjusted = Math.max(1, Number(line) - sourceLineOffset);
    return `${PROGRAM_FILENAME}:${adjusted}:${column}`;
  });
}

function serializeError(error) {
  if (error instanceof Error) {
    const payload = {
      name: error.name,
      message: error.message,
      stack: mapStackToUserCode(error.stack),
    };
    // Node fs/permission errors carry an actionable `code` (ERR_ACCESS_DENIED,
    // ENOENT, PTC_EXIT, ...); keep it so the host can surface the cause.
    if (typeof error.code === 'string') payload.code = error.code;
    return payload;
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
/* Program-failure guards                                              */
/* ------------------------------------------------------------------ */

/**
 * Keep channel failures structured instead of surfacing as a bare EOF.
 *
 * The host reports `PTC_EOF` when the child dies without a terminal message,
 * which loses the cause. These guards convert the three ways that happens —
 * `process.exit`, an uncaught exception, an unhandled rejection — into a
 * normal terminal error the host can render.
 */
function installGuards() {
  // `process.exit` inside a run_code program would kill the channel before the
  // result is written. Refuse it and let the error propagate to main().
  const realExit = process.exit.bind(process);
  process.reallyExit = realExit;
  process.exit = function refusedExit() {
    const error = new Error(
      'PTC_EXIT: process.exit() is not allowed inside run_code — it would kill the tool ' +
      'channel before your program returns. Return a value, or throw, to end the program.'
    );
    error.code = 'PTC_EXIT';
    throw error;
  };

  // Backstop for exits that bypass the override (native abort, signal, a
  // dependency holding a reference to the original exit).
  process.on('exit', (code) => {
    if (settled) return;
    const error = new Error(
      `PTC_EXIT: node exited with code ${code} before run_code returned a result`
    );
    error.code = 'PTC_EXIT';
    writeTerminalNow({ id: RESULT_ID, ok: false, error: serializeError(error) });
  });

  process.on('uncaughtException', (error) => {
    process.exitCode = 1;
    settleError(error);
  });
  process.on('unhandledRejection', (reason) => {
    process.exitCode = 1;
    settleError(reason instanceof Error ? reason : new Error(errorText(reason)));
  });
}

/**
 * Measure how far the compiled program's reported line numbers sit below the
 * program's own first line, by throwing from a throwaway function with the
 * same injected signature.
 * @returns {Promise<number>} Lines to subtract from `<anonymous>:N:` frames.
 */
async function computeSourceLineOffset() {
  if (sourceLineOffset > 0) return sourceLineOffset;
  const AsyncFunction = Object.getPrototypeOf(async function noop() {}).constructor;
  try {
    const probe = new AsyncFunction(
      ...INJECTED_BINDINGS,
      '"use strict";\nthrow new Error("ptc-line-probe");'
    );
    await probe(...INJECTED_BINDINGS.map(() => undefined));
    return 0;
  } catch (error) {
    const match = /<anonymous>:(\d+):/.exec(typeof error.stack === 'string' ? error.stack : '');
    sourceLineOffset = match ? Math.max(0, Number(match[1]) - 1) : 0;
    return sourceLineOffset;
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
  installGuards();
  installStdoutGuard();
  try {
    const code = typeof codeOrArgv === 'string'
      ? codeOrArgv
      : loadCode(Array.isArray(codeOrArgv) ? codeOrArgv : process.argv.slice(2));

    // Measure the wrapper's line offset before compiling the real program so
    // any error it throws carries the program's own line numbers.
    await computeSourceLineOffset();

    const AsyncFunction = Object.getPrototypeOf(async function noop() {}).constructor;
    // Reject an unusable host injection list before compiling anything.
    validateBindingNames(INJECTED_BINDINGS);
    const program = new AsyncFunction(...INJECTED_BINDINGS, `"use strict";\n${code}`);
    const result = await program(sdk, ptcConsole);
    settle({ id: RESULT_ID, ok: true, result: serializeValue(result) });
    return result;
  } catch (err) {
    process.exitCode = 1;
    settle({ id: RESULT_ID, ok: false, error: serializeError(err) });
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
  normalizeArgs,
  mergeScope,
  main,
  loadCode,
  serializeValue,
  serializeError,
  mapStackToUserCode,
  computeSourceLineOffset,
  validateBindingNames,
  INJECTED_BINDINGS,
  TOOL_TIMEOUT_MS,
};

if (require.main === module) {
  main();
}
