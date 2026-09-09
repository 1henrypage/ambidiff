// Placeholder views (B16): an oversized diff, a binary diff, and a
// mode-only change all go through the same one placeholder-capable path
// and none of them error or drop a comment.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;
let hugeAdds: number;

test.beforeAll(async () => {
  repo = new ScratchRepo();
  repo.write("src/huge.txt", "line\n");
  repo.write("src/binary.bin", "");
  repo.write("src/mode-only.sh", "echo hi\n");
  repo.commitAll("base");

  const hugeLines = Array.from({ length: 12_000 }, (_, i) => `generated line ${i}`);
  hugeAdds = hugeLines.length;
  repo.write("src/huge.txt", hugeLines.join("\n") + "\n");
  repo.writeBytes("src/binary.bin", Buffer.from(Array.from({ length: 64 }, (_, i) => i % 256)));
  repo.chmodExecutable("src/mode-only.sh");

  repo.cli(["init", "--review", "placeholders-e2e", "--base", "HEAD"]);
  repo.cli(["comment", "add", "-p", "src/huge.txt", "-m", "please split this up"]);

  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

test("an oversized diff shows real counts and keeps its comment", async ({ page }) => {
  await page.goto(url);
  await page.locator(".tree-row", { hasText: "huge.txt" }).click();
  await expect(page.locator("#rows")).toContainText("too large to render", { timeout: 20_000 });
  await expect(page.locator("#rows")).toContainText(`+${hugeAdds}`);
  await expect(page.locator("#rows")).toContainText("please split this up");
});

test("a binary diff shows its notice", async ({ page }) => {
  await page.goto(url);
  await page.locator(".tree-row", { hasText: "binary.bin" }).click();
  await expect(page.locator("#rows")).toContainText("(binary file)", { timeout: 20_000 });
});

test("a mode-only change shows an empty view without error", async ({ page }) => {
  await page.goto(url);
  await page.locator(".tree-row", { hasText: "mode-only.sh" }).click();
  await expect(page.locator("#banner")).toContainText("mode-only.sh", { timeout: 20_000 });
  await expect(page.locator("#statusbar .msg")).toHaveText("");
  await expect(page.locator(".dl.notice")).toHaveCount(0);
});
