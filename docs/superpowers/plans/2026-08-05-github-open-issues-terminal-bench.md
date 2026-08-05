# Terminal-Bench Open Issues Implementation Plan

**Goal:** Fix GitHub issues #22 and #23, correct the Harbor context regression reported in #25, and retain the already committed repository-hygiene fix for #24.

**Architecture:** Keep the Harbor adapter self-contained. Derive the reported Orca version from the mounted binary, preserve the command transcript as Harbor's trajectory artifact, and document only filters supported by Harbor 0.20.0.

**Tech Stack:** Python 3 standard library, Harbor adapter API, Markdown, Node repository checks.

### Task 1: Add adapter regression tests

**Files:**
- Create: `terminal_bench/test_orca_agent.py`
- Modify: `terminal_bench/orca_agent.py`

1. Stub Harbor modules so the adapter can be tested without installing Harbor.
2. Assert `version()` reads the mounted binary's `--version` output.
3. Assert `run()` writes stdout to `trajectory.jsonl` without adding fields to Harbor's closed `AgentContext` model.
4. Run `python3 -m unittest terminal_bench.test_orca_agent` and confirm the tests fail before implementation.
5. Implement the smallest adapter changes and rerun the tests.

### Task 2: Correct Harbor 0.20.0 documentation

**Files:**
- Modify: `terminal_bench/README.md`
- Modify: `terminal_bench/test_orca_agent.py`

1. Add a documentation assertion that rejects `--filter-difficulty`.
2. Replace the invalid example with supported task-name/count filtering.
3. Run the focused Python tests.

### Task 3: Verify and commit

1. Run the focused Python tests and repository hygiene test.
2. Run formatting/static checks applicable to the changed files.
3. Commit with `Fixes #22`, `Fixes #23`, and `Fixes #25` trailers.
