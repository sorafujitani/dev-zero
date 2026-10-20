#!/usr/bin/env node
"use strict";

const path = require("node:path");
const { spawnSync } = require("node:child_process");

function binaryPath(platform = process.platform, arch = process.arch) {
  if (!["darwin", "linux"].includes(platform) || !["x64", "arm64"].includes(arch)) {
    throw new Error(`Unsupported platform: ${platform}/${arch}. Use macOS or Linux on x64 or arm64.`);
  }
  return path.join(__dirname, "..", "prebuilt", `${platform}-${arch}`, "dz");
}

function main() {
  try {
    const result = spawnSync(binaryPath(), process.argv.slice(2), { stdio: "inherit" });
    if (result.error) {
      throw new Error(`Cannot start the bundled dz binary: ${result.error.message}. Reinstall dev-zero from npm.`);
    }
    if (result.signal) {
      process.kill(process.pid, result.signal);
      return;
    }
    process.exitCode = result.status ?? 1;
  } catch (error) {
    console.error(`dz: ${error.message}`);
    process.exitCode = 1;
  }
}

if (require.main === module) main();
module.exports = { binaryPath };
