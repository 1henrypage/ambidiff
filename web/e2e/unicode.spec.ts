// Byte-accurate painting (B09): accents, an emoji with a multi-byte tail,
// CJK, a combining sequence, a literal tab, and a diff line that itself
// contains `<b>&amp;` -- proving the DOM shows it as literal text (only
// possible without `innerHTML`), never as markup. Also: syntax highlight
// spans still apply to added lines, and search finds a non-ASCII query.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

const COMBINING = "é"; // "e" + COMBINING ACUTE ACCENT: renders like "é", two codepoints.

test.beforeAll(async () => {
  repo = new ScratchRepo();
  repo.write("src/greet.ts", 'function greet() {\n  return "hi";\n}\n');
  repo.commitAll("base");
  repo.write(
    "src/greet.ts",
    [
      "function greet() {",
      '  return "hi";',
      '  const cafe = "café";',
      '  const emoji = "foo 😀 tail";',
      '  const cjk = "你好世界";',
      `  const combining = "${COMBINING}";`,
      '  const tabbed = "a\tb";',
      '  const escaped = "<b>&amp;</b>";',
      "}",
      "",
    ].join("\n"),
  );
  repo.cli(["init", "--review", "unicode-e2e", "--base", "HEAD"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

test("unicode content paints byte-accurately and never as markup", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("café", { timeout: 20_000 });

  await expect(page.locator("#rows")).toContainText('const cafe = "café";');
  await expect(page.locator("#rows")).toContainText('const emoji = "foo 😀 tail";');
  await expect(page.locator("#rows")).toContainText('const cjk = "你好世界";');
  await expect(page.locator("#rows")).toContainText(`const combining = "${COMBINING}";`);
  await expect(page.locator("#rows")).toContainText('const tabbed = "a\tb";');
  // Rendered as literal text: if this had gone through innerHTML, the "<b>"
  // would have become a real bold element instead of visible text.
  await expect(page.locator("#rows")).toContainText('const escaped = "<b>&amp;</b>";');
  const bold = page.locator("#rows .dl:has-text('escaped') b");
  await expect(bold).toHaveCount(0);

  // Syntax highlight spans (inline RGB from the core) still apply to added lines.
  await expect(page.locator('#rows span[style*="color"]').first()).toBeVisible();

  // Search for a non-ASCII query lands the cursor on the right row.
  await page.locator("#viewport").click();
  await page.keyboard.press("/");
  await page.locator("#overlay input").fill("你好");
  await page.keyboard.press("Enter");
  await expect(page.locator(".dl.cursor")).toContainText("你好世界");
  await expect(page.locator(".dl.cursor .hit")).toContainText("你好");
});
