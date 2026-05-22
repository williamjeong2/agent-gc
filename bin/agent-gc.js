#!/usr/bin/env node

var childProcess = require("child_process");
var fs = require("fs");
var path = require("path");

function platformName() {
  var platform = process.platform;
  var arch = process.arch;

  if (platform === "darwin" && arch === "arm64") return "darwin-arm64";
  if (platform === "darwin" && arch === "x64") return "darwin-x64";
  if (platform === "linux" && arch === "x64") return "linux-x64";
  if (platform === "linux" && arch === "arm64") return "linux-arm64";

  return platform + "-" + arch;
}

function candidates() {
  var root = path.resolve(__dirname, "..");
  var exe = process.platform === "win32" ? ".exe" : "";
  var name = "agent-gc-" + platformName() + exe;

  return [
    path.join(root, "vendor", name),
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

var bin = findBinary();
if (!bin) {
  console.error("agent-gc binary not found. Run `cargo build --release` or install a package with vendor binaries.");
  process.exit(1);
}

var result = childProcess.spawnSync(bin, process.argv.slice(2), {
  stdio: "inherit"
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

process.exit(result.status == null ? 1 : result.status);
