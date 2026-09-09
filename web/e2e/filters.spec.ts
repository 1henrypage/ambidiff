// The file filter (B15): `f` cycles all -> annotated -> unreviewed -> all,
// and adding the first comment on a file makes it appear under "annotated"
// immediately, from the `reviewChanged` broadcast alone -- no extra refresh.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

test.beforeAll(async () => {
  repo = new ScratchRepo();
  repo.write("src/a.ts", "export const a = 1;\n");
  repo.write("src/b.ts", "export const b = 1;\n");
  repo.commitAll("base");
  repo.write("src/a.ts", "export const a = 2;\n");
  repo.write("src/b.ts", "export const b = 2;\n");
  repo.cli(["init", "--review", "filters-e2e", "--base", "HEAD"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

const fileRows = (page: import("@playwright/test").Page) => page.locator(".tree-row:has(.fstatus)");

test("f cycles the filter and a fresh comment updates it live", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("export const a", { timeout: 20_000 });
  await expect(fileRows(page)).toHaveCount(2);

  await page.locator("#viewport").click();
  await page.keyboard.press("f");
  await expect(page.locator("#statusbar")).toContainText("filter:annotated");
  await expect(fileRows(page)).toHaveCount(0);

  await page.keyboard.press("F");
  await page.locator("#overlay textarea").fill("please look at this file");
  await page.locator('#overlay [data-act="save"]').click();

  await expect(fileRows(page)).toHaveCount(1, { timeout: 15_000 });
  await expect(fileRows(page)).toContainText("a.ts");

  await page.keyboard.press("f");
  await expect(page.locator("#statusbar")).toContainText("filter:unreviewed");
  await expect(fileRows(page)).toHaveCount(1);
  await expect(fileRows(page)).toContainText("b.ts");

  await page.keyboard.press("f");
  await expect(fileRows(page)).toHaveCount(2);
});
