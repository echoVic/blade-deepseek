import { readFileSync } from "node:fs";

const root = new URL("..", import.meta.url);
const canonicalUrl = "https://orcaagent.dev/";
const indexHtml = readFileSync(new URL("index.html", root), "utf8");
const appSource = readFileSync(new URL("src/App.tsx", root), "utf8");
const styles = readFileSync(new URL("src/styles.css", root), "utf8");
const readme = readFileSync(new URL("../README.md", root), "utf8");
const robotsTxt = readFileSync(new URL("public/robots.txt", root), "utf8");
const sitemapXml = readFileSync(new URL("public/sitemap.xml", root), "utf8");
const socialPng = readFileSync(new URL("public/orca-social.png", root));

const failures = [];

function check(condition, message) {
  if (!condition) {
    failures.push(message);
  }
}

function includes(value, message) {
  check(indexHtml.includes(value), message);
}

includes(`<link rel="canonical" href="${canonicalUrl}" />`, "Missing canonical URL");
includes('<meta name="robots" content="index, follow" />', "Missing robots index directive");
includes('<meta property="og:type" content="website" />', "Missing Open Graph type");
includes(`<meta property="og:url" content="${canonicalUrl}" />`, "Missing Open Graph URL");
includes(
  '<meta property="og:image" content="https://orcaagent.dev/orca-social.png" />',
  "Missing Open Graph PNG image",
);
includes('<meta property="og:image:width" content="1200" />', "Missing Open Graph image width");
includes('<meta property="og:image:height" content="630" />', "Missing Open Graph image height");
includes('<meta name="twitter:card" content="summary_large_image" />', "Missing Twitter card");
includes(
  '<meta name="twitter:image" content="https://orcaagent.dev/orca-social.png" />',
  "Missing Twitter PNG image",
);
includes('<script type="application/ld+json">', "Missing JSON-LD block");
includes('"@type": "SoftwareApplication"', "Missing SoftwareApplication schema");
includes('"@type": "WebSite"', "Missing WebSite schema");
includes('"@type": "Organization"', "Missing Organization schema");

check(robotsTxt.includes("User-agent: *"), "robots.txt missing user agent rule");
check(robotsTxt.includes("Allow: /"), "robots.txt missing allow rule");
check(robotsTxt.includes(`Sitemap: ${canonicalUrl}sitemap.xml`), "robots.txt missing sitemap URL");

check(sitemapXml.includes("<urlset"), "sitemap.xml missing urlset");
check(sitemapXml.includes(`<loc>${canonicalUrl}</loc>`), "sitemap.xml missing canonical loc");
check(sitemapXml.includes("<lastmod>2026-06-22</lastmod>"), "sitemap.xml missing lastmod");

check(socialPng.subarray(1, 4).toString("ascii") === "PNG", "Social image is not a PNG");
check(socialPng.readUInt32BE(16) === 1200, "Social PNG width must be 1200px");
check(socialPng.readUInt32BE(20) === 630, "Social PNG height must be 630px");

check(appSource.includes("472309526"), "Homepage missing official QQ group");
check(
  appSource.includes("https://t.me/+11No1w5ZbTMyZTQ1"),
  "Homepage missing official Telegram group",
);
check(readme.includes("472309526"), "README missing official QQ group");
check(
  readme.includes("https://t.me/+11No1w5ZbTMyZTQ1"),
  "README missing official Telegram group",
);
check(
  /\.hero-copy\s*\{[^}]*align-self:\s*start;/s.test(styles),
  "Hero copy must stay top-aligned while the terminal animation grows",
);

const jsonLdBlocks = [
  ...indexHtml.matchAll(
    /<script type="application\/ld\+json">([\s\S]*?)<\/script>/g,
  ),
].map((match) => match[1].trim());

for (const [index, block] of jsonLdBlocks.entries()) {
  try {
    JSON.parse(block);
  } catch (error) {
    failures.push(`JSON-LD block ${index + 1} is invalid: ${error.message}`);
  }
}

check(jsonLdBlocks.length >= 1, "No parseable JSON-LD blocks found");

if (failures.length > 0) {
  console.error("SEO check failed:");
  for (const failure of failures) {
    console.error(`- ${failure}`);
  }
  process.exit(1);
}

console.log("SEO check passed.");
