import { spawn } from "node:child_process";
import { applyBrowserPath } from "./playwright-browsers.mjs";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";

const root = resolve(fileURLToPath(new URL("..", import.meta.url)));
const url = "http://127.0.0.1:5173";
applyBrowserPath();
const viteBin = resolve(root, "node_modules/vite/bin/vite.js");
const playwrightCli = resolve(root, "node_modules/playwright/cli.js");


async function isServerReady() {
  try {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 1_000);
    const response = await fetch(url, { signal: controller.signal });
    clearTimeout(timeout);
    return response.ok;
  } catch {
    return false;
  }
}

async function waitForServer() {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    if (await isServerReady()) {
      return;
    }
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 250));
  }
  throw new Error(`Timed out waiting for ${url}`);
}

function run(command, args, options = {}) {
  return new Promise((resolveRun) => {
    const child = spawn(command, args, {
      cwd: root,
      env: process.env,
      stdio: "inherit",
      windowsHide: true,
      ...options,
    });

    child.on("exit", (code, signal) => resolveRun({ child, code, signal }));
    child.on("error", (error) => {
      console.error(error);
      resolveRun({ child, code: 1, signal: null });
    });
  });
}

function stop(child) {
  if (!child || child.killed || child.exitCode !== null) {
    return;
  }
  child.kill();
}

let vite = null;

if (!(await isServerReady())) {
  vite = spawn(
    process.execPath,
    [viteBin, "--host", "127.0.0.1", "--port", "5173", "--strictPort"],
    {
      cwd: root,
      env: process.env,
      stdio: "inherit",
      windowsHide: true,
    },
  );
  await waitForServer();
}

try {
  const result = await run(process.execPath, [playwrightCli, "test", ...process.argv.slice(2)]);
  process.exitCode = result.code ?? (result.signal ? 1 : 0);
} finally {
  stop(vite);
}
