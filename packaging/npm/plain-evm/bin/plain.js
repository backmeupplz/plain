#!/usr/bin/env node
// Runs the plain binary from the plain-evm-<os>-<cpu> package npm installed for this machine.
const { spawnSync } = require("node:child_process");

const pkg = `plain-evm-${process.platform}-${process.arch}`;
let bin;
try {
  bin = require.resolve(`${pkg}/bin/plain`);
} catch {
  console.error(`plain: npm didn't install ${pkg}, the package with plain's binary for this machine.`);
  console.error("Binaries exist for macOS and x64 Linux with glibc 2.34+. If you installed with --omit=optional, install again without it.");
  console.error("More ways to install: https://getplain.org");
  process.exit(1);
}

process.on("SIGINT", () => {}); // Ctrl+C reaches plain too: let it exit first, then exit like it did
const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (r.error) throw r.error;
if (r.signal) {
  process.removeAllListeners(r.signal);
  process.kill(process.pid, r.signal);
}
process.exit(r.status ?? 1);
