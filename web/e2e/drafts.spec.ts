// Drafts survive failure (B19): a write is refused locally when read-only
// or disconnected, but an already-open overlay is never closed by a state
// change and its Save always attempts the write, showing the failure in
// `.overlay-error` with the typed text kept. Also: an optional empty
// response submits, an empty required body does not.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, readReview, writeReview, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

test.beforeEach(async () => {
  repo = new ScratchRepo();
  repo.write("src/a.ts", "export const a = 1;\n");
  repo.commitAll("base");
  repo.write("src/a.ts", "export const a = 2;\n");
  repo.cli(["init", "--review", "drafts-e2e", "--base", "HEAD"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterEach(() => {
  server?.stop();
  repo?.remove();
});

test("a draft survives the review turning read-only, then retries once restored", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("export const a", { timeout: 20_000 });

  await page.locator(".dl.add").click();
  await page.keyboard.press("c");
  const textarea = page.locator("#overlay textarea");
  await textarea.fill("does this need a guard?");

  // An external agent (or a corrupt write) makes the review read-only.
  const review = readReview(repo.root);
  review["ambidiff"] = 999;
  writeReview(repo.root, review);
  await expect(page.locator("#diag")).toContainText("read-only", { timeout: 20_000 });

  // The open draft is untouched by that state change.
  await expect(textarea).toHaveValue("does this need a guard?");

  await page.locator('#overlay [data-act="save"]').click();
  await expect(page.locator("#overlay .overlay-error")).toBeVisible({ timeout: 10_000 });
  await expect(textarea).toHaveValue("does this need a guard?");

  // Restored: the same Save now goes through without retyping anything.
  review["ambidiff"] = 1;
  writeReview(repo.root, review);
  await expect(page.locator("#diag")).toBeHidden({ timeout: 20_000 });

  await page.locator('#overlay [data-act="save"]').click();
  await expect(page.locator("#overlay")).toBeHidden({ timeout: 15_000 });
  await expect(page.locator("#rows")).toContainText("does this need a guard?", { timeout: 15_000 });
});

test("disconnected: the draft stays, the strip shows it, and Save reports the failure", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("export const a", { timeout: 20_000 });

  await page.locator(".dl.add").click();
  await page.keyboard.press("c");
  const textarea = page.locator("#overlay textarea");
  await textarea.fill("still drafting when the server dies");

  server.stop();

  await expect(page.locator("#diag")).toContainText("disconnected", { timeout: 20_000 });
  await expect(page.locator('#diag [data-act="reconnect"]')).toBeVisible();
  await expect(page.locator("#overlay")).toBeVisible();
  await expect(textarea).toHaveValue("still drafting when the server dies");

  await page.locator('#overlay [data-act="save"]').click();
  await expect(page.locator("#overlay .overlay-error")).toBeVisible({ timeout: 10_000 });
  await expect(textarea).toHaveValue("still drafting when the server dies");
});

test("a failed save (the target vanished) keeps the overlay and the draft", async ({ page }) => {
  const added = JSON.parse(
    repo.cli(["comment", "add", "-p", "src/a.ts", "-l", "1", "-m", "original", "--json"]),
  ) as { id: string };

  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("original", { timeout: 20_000 });

  await page.keyboard.press(".");
  await page.keyboard.press("e");
  const textarea = page.locator("#overlay textarea");
  await expect(textarea).toHaveValue("original");
  await textarea.fill("edited after the comment was deleted underneath");

  // Deleted by another process between opening the draft and saving it.
  repo.cli(["comment", "delete", added.id]);

  await page.locator('#overlay [data-act="save"]').click();
  await expect(page.locator("#overlay .overlay-error")).toBeVisible({ timeout: 10_000 });
  await expect(textarea).toHaveValue("edited after the comment was deleted underneath");
  await expect(page.locator("#overlay")).toBeVisible();
});

test("an optional empty response submits; an empty required body is refused", async ({ page }) => {
  repo.cli(["comment", "add", "-p", "src/a.ts", "-l", "1", "-m", "please check this"]);

  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("please check this", { timeout: 20_000 });

  await page.keyboard.press(".");
  await page.keyboard.press("a");
  await expect(page.locator("#overlay textarea")).toHaveValue("");
  await page.locator('#overlay [data-act="save"]').click();
  await expect(page.locator("#overlay")).toBeHidden({ timeout: 15_000 });
  await expect(page.locator("#rows .st-addressed")).toBeVisible({ timeout: 15_000 });

  await page.locator(".dl.add").click();
  await page.keyboard.press("c");
  await page.locator('#overlay [data-act="save"]').click();
  await expect(page.locator("#overlay .overlay-error")).toContainText("required");
  await expect(page.locator("#overlay")).toBeVisible();
});
