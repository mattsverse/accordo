const assert = require("node:assert/strict");
const { spawn, spawnSync } = require("node:child_process");
const { once } = require("node:events");
const { mkdtempSync, mkdirSync, copyFileSync, writeFileSync, rmSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const { test } = require("node:test");

function fixture(t, source) {
  const root = mkdtempSync(join(tmpdir(), "accordo-launcher-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const launcher = join(root, "accordo.cjs");
  copyFileSync(join(__dirname, "bin/accordo.cjs"), launcher);
  if (source !== undefined) {
    const name = `accordo-${process.platform}-${process.arch}${process.platform === "linux" ? "-gnu" : ""}`;
    const bin = join(root, "node_modules/@getaccordo", name, "bin");
    mkdirSync(bin, { recursive: true });
    writeFileSync(join(bin, "accordo"), `#!/usr/bin/env node\n${source}`, { mode: 0o755 });
  }
  return launcher;
}

test("passes arguments, stdin, stdout, stderr and failure status through", (t) => {
  const launcher = fixture(t, `
    console.log(JSON.stringify(process.argv.slice(2)));
    console.log(require('node:fs').readFileSync(0, 'utf8'));
    console.error('native stderr');
    process.exitCode = 37;
  `);
  const result = spawnSync(process.execPath, [launcher, "a b", "--flag=value", "$(literal)"], {
    input: "native stdin", encoding: "utf8", timeout: 5000,
  });
  assert.equal(result.status, 37);
  assert.deepEqual(JSON.parse(result.stdout.split("\n")[0]), ["a b", "--flag=value", "$(literal)"]);
  assert.match(result.stdout, /native stdin/);
  assert.match(result.stderr, /native stderr/);
});

test("missing optional dependency produces an actionable failure", (t) => {
  const result = spawnSync(process.execPath, [fixture(t)], { encoding: "utf8", timeout: 5000 });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /optional dependencies enabled/);
});

test("failed native executable never reports success", (t) => {
  const launcher = fixture(t, "");
  const name = `accordo-${process.platform}-${process.arch}${process.platform === "linux" ? "-gnu" : ""}`;
  writeFileSync(join(launcher, "..", "node_modules/@getaccordo", name, "bin/accordo"), "#!/does/not/exist\n");
  const result = spawnSync(process.execPath, [launcher], { encoding: "utf8", timeout: 5000 });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Could not start the native binary/);
});

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP", "SIGWINCH"]) {
  test(`forwards ${signal} to the native process and waits for cleanup`, { timeout: 5000 }, async (t) => {
    const launcher = fixture(t, `
      process.on('${signal}', () => { console.log('cleaned up'); process.exit(42); });
      console.log('ready');
      setInterval(() => {}, 1000);
    `);
    const child = spawn(process.execPath, [launcher], { stdio: ["ignore", "pipe", "pipe"] });
    t.after(() => child.kill("SIGKILL"));
    const finished = once(child, "exit");
    let output = "";
    child.stdout.on("data", (data) => { output += data; });
    await once(child.stdout, "data");
    child.kill(signal);
    assert.deepEqual(await finished, [42, null]);
    assert.match(output, /cleaned up/);
  });
}

test("preserves native signal termination", (t) => {
  const launcher = fixture(t, "process.kill(process.pid, 'SIGTERM');");
  const result = spawnSync(process.execPath, [launcher], { timeout: 5000 });
  assert.equal(result.signal, "SIGTERM");
});
