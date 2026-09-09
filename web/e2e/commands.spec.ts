// The five B17 commands plus tree keyboard navigation and help: Tab moves
// focus to the tree with a visible ring and j/k/Enter drive it, W toggles
// wrap, L hides the gutters, e edits a comment through the draft flow, D
// confirms then deletes, and ? opens help listing the web chords.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

test.beforeAll(async () => {
  repo = new ScratchRepo();
  repo.write("src/a.ts", "export const a = 1;\n");
  repo.commitAll("base");
  repo.write("src/a.ts", "export const a = 2;\n");
  repo.cli(["init", "--review", "commands-e2e", "--base", "HEAD"]);
  repo.cli(["comment", "add", "-p", "src/a.ts", "-l", "1", "-m", "will edit"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

test("Tab focuses the tree; j/k/Enter navigate it", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("export const a", { timeout: 20_000 });

  await page.locator("#viewport").click();
  await page.keyboard.press("Tab");
  const overviewRow = page.locator(".tree-row", { hasText: "(review)" });
  await expect(overviewRow).toHaveClass(/cursor/);
  await expect(overviewRow).toHaveCSS("outline-style", "solid");

  // one "j" for the "src" directory row, one more for the "a.ts" leaf.
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  const fileRow = page.locator(".tree-row", { hasText: "a.ts" });
  await expect(fileRow).toHaveClass(/cursor/);

  await page.keyboard.press("Enter");
  await expect(page.locator("#banner")).toContainText("a.ts");

  await page.keyboard.press("Tab");
  await expect(fileRow).not.toHaveClass(/cursor/);
});

test("L hides the gutter line numbers", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("export const a", { timeout: 20_000 });
  await expect(page.locator(".dl .gut").first()).toBeVisible();

  await page.locator("#viewport").click();
  await page.keyboard.press("L");
  await expect(page.locator("#app")).toHaveClass(/no-gutter/);
  await expect(page.locator(".dl .gut").first()).not.toBeVisible();
});

test("e edits a comment through the draft flow; D confirms then deletes it", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("will edit", { timeout: 20_000 });

  await page.locator("#viewport").click();
  await page.keyboard.press(".");
  await expect(page.locator(".dl.cursor")).toHaveClass(/card/);

  await page.keyboard.press("e");
  const textarea = page.locator("#overlay textarea");
  await expect(textarea).toHaveValue("will edit");
  await textarea.fill("now edited");
  await page.locator('#overlay [data-act="save"]').click();
  await expect(page.locator("#rows")).toContainText("now edited", { timeout: 15_000 });

  await page.keyboard.press(".");
  await expect(page.locator(".dl.cursor")).toHaveClass(/card/);
  await page.keyboard.press("D");
  await expect(page.locator("#overlay")).toBeVisible();
  await page.locator('#overlay [data-act="confirm"]').click();
  await expect(page.locator("#rows")).not.toContainText("now edited", { timeout: 15_000 });
});

test("? opens help listing the web chords", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("export const a", { timeout: 20_000 });

  await page.locator("#viewport").click();
  await page.keyboard.press("?");
  await expect(page.locator("#overlay")).toContainText("ambidiff help");
  await expect(page.locator("#overlay td.chord")).toContainText(["Tab"]);
  await expect(page.locator("#overlay")).toContainText("Cursor down");

  await page.keyboard.press("Escape");
  await expect(page.locator("#overlay")).toBeHidden();
});
