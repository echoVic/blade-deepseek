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
