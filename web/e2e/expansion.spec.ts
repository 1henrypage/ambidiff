// Gap expansion (B10): expanding a collapsed run of unchanged lines paints
// the new rows without a page reload, and a comment placed on a row inside
// the freshly-expanded region anchors there (not dropped, not misplaced).
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

function lines(n: number, prefix = "line"): string {
  return Array.from({ length: n }, (_, i) => `${prefix} ${i + 1}`).join("\n") + "\n";
}

test.beforeAll(async () => {
  repo = new ScratchRepo();
  const base = lines(30);
  repo.write("src/long.txt", base);
  repo.commitAll("base");
  const edited = base.replace("line 15", "line fifteen, changed");
  repo.write("src/long.txt", edited);
  repo.cli(["init", "--review", "expansion-e2e", "--base", "HEAD"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

test("expanding a gap paints its rows, and a comment inside it anchors there", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("line fifteen, changed", { timeout: 20_000 });

  const gap = page.locator(".dl.gap").first();
  await expect(gap).toBeVisible();
  await expect(page.locator("#rows")).not.toContainText("line 5");

  await gap.click();
  await expect(page.locator("#rows")).toContainText("line 5", { timeout: 15_000 });
  // the gap that was just expanded is gone; only the trailing one remains.
  await expect(page.locator(".dl.gap")).toHaveCount(1);

  const targetRow = page.locator(".dl", { hasText: /^\s*5\s+5\s+ line 5$/ });
  await expect(targetRow).toBeVisible();
  await targetRow.click();
  await page.keyboard.press("c");
  await page.locator("#overlay textarea").fill("why does this context matter?");
  await page.locator('#overlay [data-act="save"]').click();

  await expect(page.locator("#rows")).toContainText("why does this context matter?", { timeout: 15_000 });

  await expect
    .poll(() => {
      const review = JSON.parse(readFileSync(join(repo.root, ".ambidiff.json"), "utf8"));
      const c = review.comments[0];
      return `${c?.side}:${c?.line}`;
    })
    .toBe("new:5");
});
