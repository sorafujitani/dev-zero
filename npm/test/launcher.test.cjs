"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const { binaryPath } = require("../bin/dz.cjs");

test("selects each supported binary", () => {
  for (const platform of ["darwin", "linux"]) {
    for (const arch of ["x64", "arm64"]) {
      assert.ok(binaryPath(platform, arch).endsWith(`/prebuilt/${platform}-${arch}/dz`));
    }
  }
});
test("rejects unsupported platforms rather than running a different binary", () => {
  for (const [platform, arch] of [["win32", "x64"], ["linux", "ia32"], ["../../tmp", "x64"]]) {
    assert.throws(() => binaryPath(platform, arch), /Unsupported platform/);
  }
});
