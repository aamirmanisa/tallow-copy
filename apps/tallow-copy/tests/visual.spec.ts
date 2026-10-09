import { expect, test, type Page, type TestInfo } from "@playwright/test";

type SeedOptions = {
  plan?: boolean;
  history?: boolean;
  review?: boolean;
};

async function seedState(page: Page, options: SeedOptions = {}) {
  await page.evaluate((seed) => {
    const testHook = window.__tallowCopyVisualTest;
    if (!testHook) {
      throw new Error("Tallow Copy visual test hook is unavailable");
    }

    const currentState = testHook.getState();
    const request = {
      source: currentState.sourcePath,
      target: currentState.targetPath,
      mode: currentState.mode,
      filters: {
        includeGlobs: [],
        excludeGlobs: [],
        includeHidden: currentState.options.includeHidden,
        followSymlinks: currentState.options.followSymlinks,
        maxDepth: null,
      },
      verifyMode: currentState.options.verifyMode,
      metadataMode: currentState.options.metadataMode,
      backendMode: currentState.options.backendMode,
      threadCount: currentState.options.threadCount,
      bufferSizeBytes: currentState.options.bufferSizeBytes,
      deletePolicy: currentState.options.deletePolicy,
    };
    const plan = seed.plan
      ? {
          planId: "plan-visual",
          request,
          operations: [
            {
              kind: "copy",
              source: "E:/Media/a.raw",
              target: "Z:/Backup/a.raw",
              bytes: 1_048_576,
              reason: "new file",
            },
            {
              kind: "delete",
              target: "Z:/Backup/orphan.mov",
              bytes: 524_288,
              reason: "mirror target has no source match",
            },
            {
              kind: "error_risk",
              source: "E:/Media/locked.r3d",
              target: "Z:/Backup/locked.r3d",
              bytes: 2_097_152,
              reason: "locked file may need retry",
            },
          ],
          totals: {
            bytes: 3_670_016,
            files: 3,
            directories: 1,
            copies: 1,
            updates: 0,
            deletes: 1,
            verifies: 0,
            skips: 0,
            conflicts: 0,
          },
          riskSummary: {
            destructive: true,
            requiresReview: true,
            deleteCount: 1,
            conflictCount: 0,
            lockedFileCount: 1,
            estimatedErrorCount: 1,
            warnings: ["Review target deletes before executing."],
          },
        }
      : null;

    testHook.seedState({
      currentPlan: plan,
      activeView: "console",
      history: seed.history
        ? [
            {
              jobId: "job-100",
              planId: "plan-100",
              source: "E:/Media/Archive",
              target: "Z:/Backup/Media",
              mode: "mirror",
              status: "completed",
              bytesCopied: 12_884_901_888,
              bytesTotal: 17_179_869_184,
              filesCopied: 1_800,
              filesTotal: 1_900,
              filesSkipped: 100,
              errorCount: 0,
              elapsedSeconds: 92,
              averageRateBytesPerSecond: 140_053_172,
              completedAt: new Date().toISOString(),
            },
          ]
        : [],
      runtimeErrors: seed.review
        ? [
            {
              jobId: "job-100",
              planId: "plan-100",
              error: {
                path: "E:/Media/locked.r3d",
                category: "locked_file",
                message: "File is locked by another process.",
                retryable: true,
                recommendedAction: "retry",
              },
            },
          ]
        : [],
    });
  }, options);
}

async function expectNoPageOverflow(page: Page) {
  const overflow = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));

  expect(overflow.scrollWidth).toBeLessThanOrEqual(overflow.clientWidth + 1);
}

async function expectCoreShellVisible(page: Page) {
  await expect(page.locator(".toolbar")).toBeVisible();
  await expect(page.locator(".statusbar")).toBeVisible();
  await expect(page.locator(".sidebar")).toBeVisible();
}

async function captureViewport(page: Page, testInfo: TestInfo, name: string) {
  await page.screenshot({ path: testInfo.outputPath(name), fullPage: false });
}

test.describe("Tallow Copy visual layout", () => {
  test("desktop active console is stable", async ({ page }, testInfo) => {
    await page.setViewportSize({ width: 1280, height: 820 });
    await page.goto("/");

    await expectCoreShellVisible(page);
    await expect(page.locator(".path-bar")).toBeVisible();
    await expect(page.locator(".file-list")).toBeVisible();
    await expect(page.locator(".right-panel")).toBeVisible();
    await expect(page.getByRole("button", { name: "Execute" })).toBeVisible();
    await expectNoPageOverflow(page);
    await captureViewport(page, testInfo, "active-console-1280.png");
  });

  test("compact desktop keeps rows and panel separated", async ({ page }, testInfo) => {
    await page.setViewportSize({ width: 1024, height: 768 });
    await page.goto("/");

    await expectCoreShellVisible(page);
    const fileList = await page.locator(".file-list").boundingBox();
    const rightPanel = await page.locator(".right-panel").boundingBox();

    expect(fileList).not.toBeNull();
    expect(rightPanel).not.toBeNull();
    expect(fileList!.x + fileList!.width).toBeLessThanOrEqual(rightPanel!.x + 1);
    await expectNoPageOverflow(page);
    await captureViewport(page, testInfo, "active-console-1024.png");
  });

  test("plan review shows destructive gating without overflow", async ({ page }, testInfo) => {
    await page.setViewportSize({ width: 1280, height: 820 });
    await page.goto("/");
    await seedState(page, { plan: true });

    await page.locator('[data-sidebar-id="plans"]').click();
    await expect(page.locator(".plan-review")).toBeVisible();
    await expect(page.locator(".pr-group-tab")).toHaveCount(7);
    await expect(page.locator('.plan-review [data-command-id="execute"]')).toBeDisabled();
    await expectNoPageOverflow(page);
    await captureViewport(page, testInfo, "plan-review.png");
  });

  test("the empty review queue does not read as a clean audit", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 820 });
    await page.goto("/");
    await seedState(page);

    await page.locator('[data-sidebar-id="needs-review"]').click();
    // The word "Nothing needs review" used to sit under an audit report that had just found
    // differences, which reads as a contradiction. The banner speaks for the queue, so it says so.
    await expect(page.locator("#review-empty-title")).toHaveText("No runtime errors to review");
    await expect(page.locator(".report-empty p")).toContainText("not a queue");
  });

  test("needs review renders non-destructive actions", async ({ page }, testInfo) => {
    await page.setViewportSize({ width: 1280, height: 820 });
    await page.goto("/");
    await seedState(page, { plan: true, review: true });

    await page.locator('[data-sidebar-id="needs-review"]').click();
    await expect(page.locator(".review-card")).toHaveCount(4);
    await expect(page.getByRole("button", { name: "Open plan review" })).toHaveCount(3);
    await expect(page.getByRole("button", { name: "Back to active job" })).toHaveCount(1);
    await expectNoPageOverflow(page);
    await captureViewport(page, testInfo, "needs-review.png");
  });
});
