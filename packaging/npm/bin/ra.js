#!/usr/bin/env node
// Launcher: spawn the real native `ra` binary that postinstall placed in
// vendor/, forwarding all args, stdio, and the exit code.
//
// The skills bundled next to it (vendor/news_fetch, deep-search, ...) are
// discovered by `ra serve` as siblings of this binary at runtime — so the
// native binary must run from vendor/, not be copied elsewhere.

"use strict";

const os = require("os");
const path = require("path");
const fs = require("fs");
const { spawnSync } = require("child_process");

const exeSuffix = os.platform() === "win32" ? ".exe" : "";
const binary = path.join(__dirname, "..", "vendor", "ra" + exeSuffix);

if (!fs.existsSync(binary)) {
  console.error(
    "[@icehomura/RecurAgent] native binary not found at " +
      binary +
      ".\nThe postinstall download did not run (was the package installed with " +
      "--ignore-scripts?).\nReinstall without --ignore-scripts, or run " +
      "`node " +
      path.join(__dirname, "..", "install.js") +
      "` manually."
  );
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  console.error("[@icehomura/RecurAgent] failed to launch ra: " + result.error.message);
  process.exit(1);
}

// Propagate a signal-terminated child as a non-zero exit; otherwise the code.
if (result.signal) {
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
