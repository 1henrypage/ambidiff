// Layout: the fixed 20px layout and the wrapped MeasuredLayout must never
// let two rows overlap, and a wrapped long line must actually grow taller
// than the fixed row height. Exercises `W` under the default window, a
// narrow window, and split mode.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

const LONG_LINE = "x".repeat(400);

const WORDS =
  "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike " +
  "november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu";
const LONG_COMMENT_BODY = `${WORDS}\n\nsecond paragraph ${WORDS}`;
const LONG_RESPONSE = `response ${WORDS}`;

test.beforeAll(async () => {
  repo = new ScratchRepo();
  repo.write("src/base.ts", "line one\nline two\nline three\n");
  repo.commitAll("base");
  repo.write("src/base.ts", `line one\n${LONG_LINE}\nline two\nline three\n`);
  repo.cli(["init", "--review", "layout-e2e", "--base", "HEAD"]);
  // A long multi-word, two-paragraph comment plus a long response, added
  // and addressed through the CLI the way an agent would (the id
  // round-trip pattern from browser.spec.ts).
  const created = JSON.parse(
    repo.cli(["comment", "add", "-p", "src/base.ts", "-m", LONG_COMMENT_BODY, "--json"]),
  ) as { id: string };
  repo.cli(["comment", "addressed", created.id, "-m", LONG_RESPONSE]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

/** No two consecutive `.dl` rows may occupy overlapping vertical space. */
async function assertNoOverlap(page: import("@playwright/test").Page): Promise<{ tallestHeight: number }> {
  const rows = page.locator(".dl[data-index]");
  const count = await rows.count();
  let prevBottom: number | null = null;
  let tallestHeight = 0;
  for (let i = 0; i < count; i++) {
    const box = await rows.nth(i).boundingBox();
    if (!box) continue;
    if (prevBottom !== null) expect(box.y + 0.5).toBeGreaterThanOrEqual(prevBottom);
    prevBottom = box.y + box.height;
    tallestHeight = Math.max(tallestHeight, box.height);
  }
  return { tallestHeight };
}

test("W switches to a measured layout with no overlapping rows, then back", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("line one", { timeout: 20_000 });

  const fixedBox = await page.locator(".dl[data-index]").first().boundingBox();
  expect(fixedBox?.height ?? 0).toBeLessThan(21);
  await assertNoOverlap(page);

  await page.locator("#viewport").click();
  await page.keyboard.press("W");
  await expect(page.locator("#app")).toHaveClass(/wrap/);

  const { tallestHeight } = await assertNoOverlap(page);
  expect(tallestHeight).toBeGreaterThan(20.5);

  await page.keyboard.press("W");
  await expect(page.locator("#app")).not.toHaveClass(/wrap/);
  const afterBox = await page.locator(".dl[data-index]").first().boundingBox();
  expect(afterBox?.height ?? 0).toBeLessThan(21);
  await assertNoOverlap(page);
});

test("wrap holds under a narrow window", async ({ page }) => {
  await page.setViewportSize({ width: 420, height: 600 });
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("line one", { timeout: 20_000 });
  await page.locator("#viewport").click();
  await page.keyboard.press("W");
  await expect(page.locator("#app")).toHaveClass(/wrap/);
  const { tallestHeight } = await assertNoOverlap(page);
  expect(tallestHeight).toBeGreaterThan(20.5);
});

test("wrap holds in split mode", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("line one", { timeout: 20_000 });
  await page.locator("#viewport").click();
  await page.keyboard.press("s");
  await expect(page.locator(".split-sep").first()).toBeVisible({ timeout: 15_000 });
  await page.keyboard.press("W");
  await expect(page.locator("#app")).toHaveClass(/wrap/);
  const { tallestHeight } = await assertNoOverlap(page);
  expect(tallestHeight).toBeGreaterThan(20.5);
});

test("wrap grows a card's rows while its head stays a single line, with no horizontal overflow", async ({
  page,
}) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("line one", { timeout: 20_000 });

  const fixedClineHeight = (await page.locator(".dl.card.cline").first().boundingBox())?.height ?? 0;
  expect(fixedClineHeight).toBeLessThanOrEqual(21);

  await page.locator("#viewport").click();
  await page.keyboard.press("W");
  await expect(page.locator("#app")).toHaveClass(/wrap/);

  const wrappedCline = page.locator(".dl.card.cline").first();
  await expect(async () => {
    const box = await wrappedCline.boundingBox();
    expect(box?.height ?? 0).toBeGreaterThan(20.5);
  }).toPass({ timeout: 15_000 });

  const cheadHeight = (await page.locator(".dl.card.chead").first().boundingBox())?.height ?? 0;
  expect(cheadHeight).toBeLessThanOrEqual(21);

  await assertNoOverlap(page);

  // The card text reaching the DOM as a bare text node (rather than
  // inside `.celltext`) is exactly what let it overflow horizontally
  // instead of wrapping - this fails on the old markup and passes now.
  const overflow = await page
    .locator("#viewport")
    .evaluate((el) => el.scrollWidth - el.clientWidth);
  expect(overflow).toBeLessThanOrEqual(1);

  // The left rule must run the full height of a multi-line row, not just
  // one glyph's worth at the top.
  const borderBox = await wrappedCline.locator(".border").boundingBox();
  const rowBox = await wrappedCline.boundingBox();
  expect(borderBox?.height ?? 0).toBeCloseTo(rowBox?.height ?? 0, 0);

  await page.keyboard.press("W");
  await expect(page.locator("#app")).not.toHaveClass(/wrap/);
  const restoredClineHeight = (await page.locator(".dl.card.cline").first().boundingBox())?.height ?? 0;
  const restoredCheadHeight = (await page.locator(".dl.card.chead").first().boundingBox())?.height ?? 0;
  expect(restoredClineHeight).toBeLessThanOrEqual(21);
  expect(restoredCheadHeight).toBeLessThanOrEqual(21);
});

test("a wrapped response line hangs its continuation under the text, not the arrow", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("line one", { timeout: 20_000 });
  await page.locator("#viewport").click();
  await page.keyboard.press("W");
  await expect(page.locator("#app")).toHaveClass(/wrap/);

  const hangLine = page.locator(".celltext.hang").first();
  await expect(hangLine).toBeVisible({ timeout: 15_000 });
  const indent = await hangLine.evaluate((el) => getComputedStyle(el).textIndent);
  expect(indent.startsWith("-")).toBe(true);
});
