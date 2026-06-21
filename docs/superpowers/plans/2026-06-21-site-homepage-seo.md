# Site Homepage SEO Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add complete SEO foundations for the Orca homepage at `https://orcaagent.dev/`.

**Architecture:** Keep the existing Vite + React homepage and visual design intact. Add static crawl and social metadata in `site/index.html`, crawler assets in `site/public/`, a small SEO regression checker, and dynamic language-specific head updates from the existing locale state.

**Tech Stack:** Vite, React, TypeScript, plain CSS, Node.js script checks.

## Global Constraints

- Do not touch the existing unrelated Rust changes in `crates/orca-tools/`.
- Use `https://orcaagent.dev/` as the canonical homepage URL.
- Keep runtime network-free.
- Do not change the homepage visual layout except for semantic attributes required for SEO/accessibility.

---

### Task 1: SEO Regression Checker

**Files:**
- Create: `site/scripts/check-seo.mjs`
- Modify: `site/package.json`

**Interfaces:**
- Produces: `npm run check:seo`, a zero-dependency Node.js check that validates static SEO metadata and crawler assets.

- [ ] **Step 1: Write the failing SEO check**

Create `site/scripts/check-seo.mjs` with assertions for canonical URL, robots meta, Open Graph, Twitter Card, JSON-LD, `robots.txt`, and `sitemap.xml`.

- [ ] **Step 2: Run the check to verify it fails**

Run: `npm run check:seo`
Expected: FAIL because the current homepage lacks the new SEO signals.

- [ ] **Step 3: Add the package script**

Add `"check:seo": "node scripts/check-seo.mjs"` to `site/package.json`.

### Task 2: Static Homepage SEO

**Files:**
- Modify: `site/index.html`
- Create: `site/public/robots.txt`
- Create: `site/public/sitemap.xml`

**Interfaces:**
- Consumes: canonical URL `https://orcaagent.dev/`.
- Produces: crawlable static homepage metadata before React hydrates.

- [ ] **Step 1: Add canonical, robots, OG, Twitter, icons, and JSON-LD to `site/index.html`**

Use `SoftwareApplication`, `WebSite`, and `Organization` JSON-LD.

- [ ] **Step 2: Add crawler files**

Create `robots.txt` with sitemap reference and `sitemap.xml` containing the homepage.

- [ ] **Step 3: Run `npm run check:seo`**

Expected: PASS.

### Task 3: Dynamic Locale Head Sync

**Files:**
- Modify: `site/src/App.tsx`

**Interfaces:**
- Consumes: existing `locale` state and `copy` object.
- Produces: matching document title, meta description, OG locale, and canonical state when the user switches English/Chinese.

- [ ] **Step 1: Add locale SEO copy and a helper to update or create meta tags**

Keep all values deterministic and local.

- [ ] **Step 2: Wire helper into the existing locale `useEffect`**

Update `document.documentElement.lang`, `document.title`, description, OG/Twitter metadata, and canonical.

- [ ] **Step 3: Run `npm run build` and rendered DOM checks**

Expected: build succeeds and rendered DOM contains canonical, JSON-LD, and locale-specific meta values.
