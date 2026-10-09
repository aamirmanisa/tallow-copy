// Which browser install Playwright should use.
//
// Playwright reads PLAYWRIGHT_BROWSERS_PATH, or falls back to its own cache when that is unset. The
// vendored path below has already fooled this suite once: the directory exists and even lists a
// chromium_headless_shell-1223 folder, but the executable inside it is missing, so Playwright fails
// before a page loads with an error naming a path nobody in this repo set.
//
// Directory existence is not usability, so a candidate counts only when the binary is really there.
// If none of them can run a browser, leave the variable unset and let Playwright report its own
// missing-browser error, which at least names the default location and the install command.

import { existsSync, readdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { homedir } from "node:os";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

// Browsers live in `chromium_headless_shell-<rev>/<platform>/chrome-headless-shell` for headless runs and
// `chromium-<rev>/<platform>/chrome` for headed ones; the platform directory name varies, so scan one
// level rather than hardcoding x64.
const MARKERS = ["chrome-headless-shell", "chrome"];

function hasRunnableBrowser(base) {
  if (!existsSync(base)) return false;
  let revisions = [];
  try {
    revisions = readdirSync(base);
  } catch {
    return false;
  }
  for (const revision of revisions) {
    if (!revision.startsWith("chromium")) continue;
    const revisionDir = join(base, revision);
    let platforms = [];
    try {
      platforms = readdirSync(revisionDir);
    } catch {
      continue;
    }
    for (const platform of platforms) {
      for (const marker of MARKERS) {
        if (existsSync(join(revisionDir, platform, marker))) return true;
      }
    }
  }
  return false;
}

export function browserCandidatePath() {
  const candidates = [
    resolve(here, "../../../tmp/ms-playwright"), // vendored, used when it is actually installed
    resolve(homedir(), ".cache/ms-playwright"), // Playwright's own default location
  ];
  for (const candidate of candidates) {
    if (hasRunnableBrowser(candidate)) return candidate;
  }
  return null;
}

export function applyBrowserPath() {
  if (process.env.PLAYWRIGHT_BROWSERS_PATH) return process.env.PLAYWRIGHT_BROWSERS_PATH;
  const chosen = browserCandidatePath();
  if (chosen) process.env.PLAYWRIGHT_BROWSERS_PATH = chosen;
  return chosen;
}
