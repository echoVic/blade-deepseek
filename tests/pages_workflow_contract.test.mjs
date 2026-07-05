import { readFileSync } from "node:fs";
import test from "node:test";
import assert from "node:assert/strict";

const workflow = readFileSync(".github/workflows/pages.yml", "utf8");

test("Pages workflow uses Node 24 compatible GitHub actions", () => {
  const expectedActions = [
    "actions/checkout@v7",
    "actions/setup-node@v6",
    "actions/configure-pages@v6",
    "actions/upload-pages-artifact@v5",
    "actions/deploy-pages@v5",
  ];

  for (const action of expectedActions) {
    assert.match(workflow, new RegExp(`uses:\\s+${action.replace("/", "\\/")}`));
  }

  assert.match(workflow, /node-version:\s+24/);
});

test("Pages deployment retries the transient GitHub Pages failure once", () => {
  assert.match(workflow, /id:\s+deployment[\s\S]*?continue-on-error:\s+true/);
  assert.match(workflow, /id:\s+deployment_retry/);
  assert.match(workflow, /if:\s+\$\{\{\s*steps\.deployment\.outcome == 'failure'\s*\}\}/);
  assert.match(
    workflow,
    /url:\s+\$\{\{\s*steps\.deployment\.outputs\.page_url \|\| steps\.deployment_retry\.outputs\.page_url\s*\}\}/,
  );
});
