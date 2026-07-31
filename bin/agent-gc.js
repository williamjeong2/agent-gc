#!/usr/bin/env node

var childProcess = require("child_process");
var fs = require("fs");
var path = require("path");

var SUPPORTED = {
  "darwin-arm64": true,
  "darwin-x64": true,
  "linux-x64": true,
  "linux-arm64": true,
  "win32-x64": true,
  "win32-arm64": true
};

function platformName() {
  var platform = process.platform;
  var arch = process.arch;

  if (platform === "darwin" && arch === "arm64") return "darwin-arm64";
  if (platform === "darwin" && arch === "x64") return "darwin-x64";
  if (platform === "linux" && arch === "x64") return "linux-x64";
  if (platform === "linux" && arch === "arm64") return "linux-arm64";
  if (platform === "win32" && arch === "x64") return "win32-x64";
  if (platform === "win32" && arch === "arm64") return "win32-arm64";

  return platform + "-" + arch;
}

function binaryBaseName(platform) {
  var exe = process.platform === "win32" ? ".exe" : "";
  return "agent-gc-" + platform + exe;
}

function candidates() {
  var root = path.resolve(__dirname, "..");
  var platform = platformName();
  var exe = process.platform === "win32" ? ".exe" : "";
  var vendorName = binaryBaseName(platform);

  return [
    path.join(root, "vendor", vendorName),
    path.join(root, "target", "release", "agent-gc" + exe),
    path.join(root, "target", "debug", "agent-gc" + exe)
  ];
}

function findBinary() {
  var list = candidates();
  for (var i = 0; i < list.length; i += 1) {
    if (fs.existsSync(list[i])) return list[i];
  }
  return null;
}

function printMissingBinaryHelp(platform) {
  var vendorName = binaryBaseName(platform);
  var supportedHint = SUPPORTED[platform]
    ? "This platform is supported; the release package may not include its binary yet."
    : "This platform is not in the primary support matrix yet.";

  console.error(
    "agent-gc native binary not found for " +
      platform +
      ".\n" +
      supportedHint +
      "\n\n" +
      "Looked for:\n" +
      "  - vendor/" +
      vendorName +
      "\n" +
      "  - target/release/agent-gc" +
      (process.platform === "win32" ? ".exe" : "") +
      "\n" +
      "  - target/debug/agent-gc" +
      (process.platform === "win32" ? ".exe" : "") +
      "\n\n" +
      "Build from source on this machine:\n" +
      "  cargo build --release\n" +
      "  # Unix: ./scripts/vendor-current.sh\n" +
      "  # or copy target/release/agent-gc to vendor/" +
      vendorName +
      "\n\n" +
      "Supported release targets:\n" +
      "  " +
      Object.keys(SUPPORTED).join(", ") +
      "\n" +
      "GitHub: https://github.com/williamjeong2/agent-gc/releases"
  );
}

var platform = platformName();
var bin = findBinary();
if (!bin) {
  printMissingBinaryHelp(platform);
  process.exit(1);
}

var result = childProcess.spawnSync(bin, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: true
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

process.exit(result.status == null ? 1 : result.status);
