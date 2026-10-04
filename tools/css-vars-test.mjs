// Every CSS custom property the UI reads without a fallback is
// defined somewhere: an undefined one is not an error to the browser,
// it silently drops the declaration.
// Run: node tools/css-vars-test.mjs   (exits non-zero on failure)
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";

const dir = new URL("../ui-assets/", import.meta.url);
const sources = readdirSync(dir)
  .filter((f) => /\.(css|js|html)$/.test(f))
  .map((f) => readFileSync(new URL(f, dir), "utf8"))
  .join("\n");

const defined = new Set([...sources.matchAll(/(--[\w-]+)\s*:/g)].map((m) => m[1]));
// `var(--name)` with no fallback; a name built in a template literal
// (`--cat-${…}`) ends in "-" and is checked where it is spelled out.
const bare = new Set(
  [...sources.matchAll(/var\(\s*(--[\w-]+)\s*\)/g)].map((m) => m[1]).filter((name) => !name.endsWith("-")),
);
const undefinedVars = [...bare].filter((name) => !defined.has(name)).sort();
assert.deepEqual(undefinedVars, [], `used with no fallback but never defined: ${undefinedVars.join(", ")}`);

console.log("css-vars-test: all assertions passed");
