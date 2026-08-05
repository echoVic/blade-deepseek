import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import test from "node:test";

const repositoryRoot = new URL("..", import.meta.url);

test("Python and benchmark artifacts stay out of the repository", () => {
  const trackedFiles = execFileSync("git", ["ls-files", "-z"], {
    cwd: repositoryRoot,
    encoding: "utf8",
  })
    .split("\0")
    .filter(Boolean);
  const trackedArtifacts = trackedFiles.filter(
    (file) =>
      /(^|\/)__pycache__\//u.test(file) ||
      /\.py[co]$/u.test(file) ||
      /(^|\/)[^/]+\.egg-info\//u.test(file) ||
      file.startsWith("jobs/"),
  );

  assert.deepEqual(trackedArtifacts, []);

  const samples = [
    "terminal_bench/__pycache__/probe.pyc",
    "terminal_bench_orca.egg-info/PKG-INFO",
    "jobs/probe/result.json",
  ];
  const ignored = spawnSync("git", ["check-ignore", "--no-index", "--stdin"], {
    cwd: repositoryRoot,
    encoding: "utf8",
    input: `${samples.join("\n")}\n`,
  });

  assert.equal(ignored.status, 0, ignored.stderr);
  assert.deepEqual(ignored.stdout.trim().split("\n"), samples);
});
