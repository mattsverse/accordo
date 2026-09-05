#!/usr/bin/env node
"use strict";

const { spawn } = require("node:child_process");

function main() {
  const { platform, arch } = process;
  if (!["darwin", "linux"].includes(platform) || !["arm64", "x64"].includes(arch)) {
    throw new Error(`No Accordo binary is available for ${platform}-${arch}.`);
  }
  if (platform === "linux" && !process.report.getReport().header.glibcVersionRuntime) {
    throw new Error("Accordo's Linux npm binaries require glibc; musl/Alpine is unsupported.");
  }
  const name = `@getaccordo/accordo-${platform}-${arch}${platform === "linux" ? "-gnu" : ""}`;
  let binary;
  try {
    binary = require.resolve(`${name}/bin/accordo`);
  } catch {
    throw new Error(`Missing ${name}. Reinstall @matfire/accordo with optional dependencies enabled.`);
  }

  // Keep the terminal attached and forward signals sent directly to this launcher.
  const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
  const handlers = new Map();
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP", "SIGWINCH"]) {
    const handler = () => child.kill(signal);
    handlers.set(signal, handler);
    process.on(signal, handler);
  }
  const cleanup = () => {
    for (const [signal, handler] of handlers) process.removeListener(signal, handler);
  };
  child.on("error", (error) => {
    cleanup();
    console.error(`accordo: Could not start the native binary: ${error.message}`);
    process.exitCode = 1;
  });
  child.on("exit", (code, signal) => {
    cleanup();
    if (signal) process.kill(process.pid, signal);
    else process.exitCode = code ?? 1;
  });
}

try {
  main();
} catch (error) {
  console.error(`accordo: ${error.message}`);
  process.exitCode = 1;
}
