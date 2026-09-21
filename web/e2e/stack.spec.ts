// Stacked-PR review in the browser: the target strip lists every PR of the
// stack, `(` / `)` and the picker move between them with the rows and the
// status label following, a comment made on a PR is tagged with it, every
// tab of one server follows the selection, and a comment whose branch left
// the stack keeps its "was on" badge.
import { test, expect } from "@playwright/test";

import { ScratchRepo, readReview, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

test.beforeEach(async () => {
  repo = new ScratchRepo();
  repo.write("src/a.ts", "export const A = 'BASE_ALPHA';\n");
  repo.commitAll("base");
  repo.git(["checkout", "-q", "-b", "auth-1"]);
  repo.write("src/a.ts", "export const A = 'AUTH_ONE_BRAVO';\n");
  repo.commitAll("add bravo");
  repo.git(["checkout", "-q", "-b", "auth-2"]);
  repo.write("src/b.ts", "export const B = 'AUTH_TWO_CHARLIE';\n");
  repo.commitAll("add charlie");
  repo.cli(["init", "--review", "stack-e2e", "--stack", "--upstream", "main"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterEach(() => {
  server?.stop();
  repo?.remove();
});

test("the strip lists the stack and ( ) walk it with the rows following", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("AUTH_TWO_CHARLIE", { timeout: 20_000 });
  const strip = page.locator("#strip");
  await expect(strip).toBeVisible();
  await expect(strip.locator(".target")).toHaveText(["1 auth-1", "[2 auth-2]", "stack"]);
  await expect(strip.locator(".target.selected")).toHaveText("[2 auth-2]");
  await expect(strip.locator(".subject")).toHaveText("add charlie");
  await expect(page.locator("#statusbar")).toContainText("target:auth-2");

  await page.locator("#viewport").click();
  await page.keyboard.press("(");
  await expect(page.locator("#rows")).toContainText("AUTH_ONE_BRAVO", { timeout: 20_000 });
  await expect(strip.locator(".target.selected")).toHaveText("[1 auth-1]");
  await expect(page.locator("#statusbar")).toContainText("target:auth-1");
  await expect(page.locator("#banner")).toContainText("a.ts");

  await page.keyboard.press("(");
  await expect(page.locator("#statusbar .msg")).toContainText("bottom of stack");

  await page.keyboard.press(")");
  await expect(page.locator("#rows")).toContainText("AUTH_TWO_CHARLIE", { timeout: 20_000 });
  await expect(strip.locator(".target.selected")).toHaveText("[2 auth-2]");
});

test("the picker lists subjects and enter selects; a comment carries its target", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("AUTH_TWO_CHARLIE", { timeout: 20_000 });
  await page.locator("#viewport").click();

  await page.keyboard.press("p");
  const picker = page.locator("#overlay .picker-row");
  await expect(picker).toHaveCount(3);
  await expect(picker.nth(0)).toContainText("add bravo");
  await expect(picker.nth(1)).toContainText("add charlie");
  await expect(picker.nth(1)).toHaveClass(/cursor/);
  await page.keyboard.press("k");
  await expect(picker.nth(0)).toHaveClass(/cursor/);
  await page.keyboard.press("Enter");
  await expect(page.locator("#overlay")).toBeHidden();
  await expect(page.locator("#rows")).toContainText("AUTH_ONE_BRAVO", { timeout: 20_000 });
  await expect(page.locator("#strip .target.selected")).toHaveText("[1 auth-1]");

  // A line comment on PR 1: land on the added line and comment.
  await page.locator("#viewport").click();
  await page.keyboard.press("]");
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  await page.keyboard.press("c");
  await expect(page.locator("#overlay b")).toContainText("comment on src/a.ts");
  await page.locator("#overlay textarea").fill("bravo??");
  await page.locator('#overlay [data-act="save"]').click();
  await expect(page.locator("#rows")).toContainText("bravo??", { timeout: 15_000 });

  const review = readReview(repo.root);
  const comments = review["comments"] as { path: string; target: unknown; snippet: string }[];
  expect(comments).toHaveLength(1);
  expect(comments[0]?.path).toBe("src/a.ts");
  expect(comments[0]?.target).toEqual({ kind: "branch", name: "auth-1" });
  expect(comments[0]?.snippet).toBe("export const A = 'AUTH_ONE_BRAVO';");
  await expect(page.locator("#strip .target.selected")).toContainText("○1/1");

  // Viewed from PR 2 the comment is hidden; the strip still counts it on PR 1.
  await page.keyboard.press(")");
  await expect(page.locator("#rows")).toContainText("AUTH_TWO_CHARLIE", { timeout: 20_000 });
  await expect(page.locator("#rows")).not.toContainText("bravo??");
  await expect(page.locator("#strip .target").nth(0)).toContainText("○1/1");
});

test("a second tab follows the selection and a strip click selects", async ({ browser, page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("AUTH_TWO_CHARLIE", { timeout: 20_000 });
  const other = await browser.newPage();
  await other.goto(url);
  await expect(other.locator("#rows")).toContainText("AUTH_TWO_CHARLIE", { timeout: 20_000 });

  await page.locator('#strip .target[data-target="auth-1"]').click();
  await expect(page.locator("#rows")).toContainText("AUTH_ONE_BRAVO", { timeout: 20_000 });
  await expect(page.locator("#strip .target.selected")).toHaveText("[1 auth-1]");

  // The selection is per server process: the other tab follows.
  await expect(other.locator("#strip .target.selected")).toHaveText("[1 auth-1]", { timeout: 20_000 });
  await expect(other.locator("#rows")).toContainText("AUTH_ONE_BRAVO", { timeout: 20_000 });
  await expect(other.locator("#statusbar")).toContainText("target:auth-1");
  await other.close();
});

test("a comment whose branch left the stack shows its was-on badge", async ({ page }) => {
  repo.cli(["comment", "add", "-p", "src/a.ts", "-l", "1", "-m", "still relevant??", "--target", "auth-1"]);
  // The bottom PR lands: main fast-forwards onto it and the branch goes.
  repo.git(["branch", "-f", "main", "auth-1"]);
  repo.git(["branch", "-D", "auth-1"]);

  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("AUTH_TWO_CHARLIE", { timeout: 20_000 });
  await expect(page.locator("#strip .target")).toHaveText(["[1 auth-2]", "stack"]);

  await page.locator(".tree-row", { hasText: "(review)" }).click();
  await expect(page.locator("#rows")).toContainText("targets no longer in the stack", { timeout: 15_000 });
  await expect(page.locator("#rows")).toContainText("still relevant??");
  await expect(page.locator("#rows .obadge", { hasText: "was on auth-1" })).toBeVisible();
});
