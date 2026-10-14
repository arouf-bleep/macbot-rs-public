#!/usr/bin/env node
// Parse the dashboard's JavaScript. The bulk now lives in standalone files;
// inline blocks are still checked so extracting one back into the page cannot
// silently escape this check, and a page with no scripts at all fails rather
// than passing vacuously.

import { readFileSync } from "node:fs";
import vm from "node:vm";

const pages = ["static/index.html"];
const modules = ["static/dashboard.js", "static/api-auth.js"];

let parsed = 0;

for (const page of pages) {
  const html = readFileSync(page, "utf8");
  const inlineScripts = [...html.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/gi)];
  for (const [index, match] of inlineScripts.entries()) {
    new vm.Script(match[1], { filename: `${page}:inline-${index + 1}` });
  }
  const referenced = [...html.matchAll(/<script[^>]*\bsrc="\/static\/([^"]+)"/gi)].map((m) => m[1]);
  const missing = modules
    .map((m) => m.replace(/^static\//, ""))
    .filter((m) => !referenced.includes(m));
  if (missing.length) {
    throw new Error(`${page} no longer loads: ${missing.join(", ")}`);
  }
  parsed += inlineScripts.length;
  console.log(`${page}: ${inlineScripts.length} inline script(s) parsed, ${referenced.length} referenced`);
}

for (const file of modules) {
  const source = readFileSync(file, "utf8");
  new vm.Script(source, { filename: file });
  parsed += 1;
  console.log(`${file}: parsed (${source.split("\n").length} lines)`);
}

if (parsed === 0) {
  throw new Error("no JavaScript was parsed; the check would pass vacuously");
}
