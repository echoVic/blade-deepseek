import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";

const root = path.resolve(import.meta.dirname, "..");
const workflow = readFileSync(path.join(root, ".github", "workflows", "release.yml"), "utf8");

test("release requires npm auth before creating public assets", () => {
  assert.match(workflow, /Verify repository version sync\n\s+run: node scripts\/release\/verify-version-sync\.mjs/);
  assert.match(workflow, /npm-auth:[\s\S]*npm whoami/);
  assert.match(workflow, /release:[\s\S]*needs: \[build, version, npm-auth\]/);
  assert.doesNotMatch(workflow, /NPM_TOKEN is not configured; npm publish skipped/);
});

test("npm publishes the five immutable tarballs native-first and main-last", () => {
  const publish = workflow.slice(workflow.indexOf("- name: Publish npm packages"), workflow.indexOf("npm-release-assets:"));
  const names = ["darwin-arm64.tgz", "darwin-x64.tgz", "linux-arm64.tgz", "linux-x64.tgz", "${version}.tgz"];
  let cursor = -1;
  for (const name of names) {
    const next = publish.indexOf(name, cursor + 1);
    assert.ok(next > cursor, `${name} must appear in publication order`);
    cursor = next;
  }
  assert.match(publish, /npm publish "\$tarball"/);
  assert.match(publish, /registry_integrity/);
  assert.match(publish, /registry_output.*E404/s);
  assert.match(publish, /already published with identical integrity/);
});

test("final verification always runs and rejects a partial publication", () => {
  assert.match(workflow, /verify:\n\s+if: \$\{\{ always\(\) && github\.ref_type == 'tag' \}\}/);
  for (const variable of ["RELEASE_RESULT", "NPM_RESULT", "ASSETS_RESULT"]) {
    assert.match(workflow, new RegExp(`test "\\$${variable}" = success`));
  }
});
