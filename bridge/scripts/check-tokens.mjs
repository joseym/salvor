#!/usr/bin/env node
// Build-time CSS token gates for bridge/.
//
// Adapts the repo's standing CSS invariants (first enforced by hand during
// the design audit, which found and deleted duplicate/dead token layers
// TWICE) into a script that runs before every build. Wire point: this is
// invoked from `npm run build` (see package.json's "build" script, which
// runs this before `ng build`); a CI workflow gains the same coverage for
// free by running `npm run build` or `npm run gate:tokens` directly. There
// is no bridge/-specific CI job yet (see ../../.github/workflows/ci.yml,
// which is Rust-workspace-only); this comment marks where a future
// `bridge` job would add a `npm run gate:tokens` (or `npm run build`) step.
//
// Rules enforced:
//   1. No literal `white`/`black` keyword or #fff.../#000... hex used as a
//      color-mix() component. The design system's whole premise is warm
//      paper/ink, never a cold literal; two exceptions are load-bearing
//      (see styles/tokens.css) and are allowlisted by an inline
//      `ANCHOR-EXCEPTION` marker comment on the line itself or the two
//      lines above it; anything else fails.
//   2. No `transition: all` (or `transition-property: all`) anywhere:
//      broad transitions silently animate properties nobody asked for.
//   3. Each of the six --anchor-* custom properties is DEFINED exactly
//      once across the whole src tree. Everything else must be a var()
//      reference or a color-mix() derived from one.

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, extname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(fileURLToPath(import.meta.url), '..', '..');
const SRC = join(ROOT, 'src');

/** @returns {string[]} absolute paths of every .css file under `dir` */
function collectCssFiles(dir) {
  const out = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    const stat = statSync(full);
    if (stat.isDirectory()) {
      out.push(...collectCssFiles(full));
    } else if (stat.isFile() && extname(full) === '.css') {
      out.push(full);
    }
  }
  return out;
}

const ANCHOR_EXCEPTION_MARKER = 'ANCHOR-EXCEPTION';
const ANCHOR_EXCEPTION_LOOKBACK = 6; // lines; the marker sits in the comment above the declaration
const LITERAL_MIX_RE = /\b(white|black)\b|#(?:fff+|000+)\b/i;
const TRANSITION_ALL_RE = /\btransition(?:-property)?\s*:\s*all\b/i;
// Matches an --anchor-* DEFINITION (`--anchor-foo:`) anywhere on the line:
// not anchored to line start, so `:root { --anchor-foo: ... }` on one line
// is still caught, while a negative lookbehind excludes `var(--anchor-foo)`
// USAGES, which are not definitions.
const ANCHOR_DEF_RE = /(?<!var\()(--anchor-[a-z0-9-]+)\s*:/gi;

/** Blank out /* ... *\/ comment bodies while preserving line breaks (and
 * therefore line numbers), so prose in comments cannot trip the code-level
 * regexes below (e.g. a comment that quotes `transition: all` to explain
 * why it is banned must not itself fail the gate). */
function stripComments(text) {
  return text.replace(/\/\*[\s\S]*?\*\//g, (block) => block.replace(/[^\n]/g, ' '));
}

function extractColorMixSpans(line) {
  const spans = [];
  let i = 0;
  while (true) {
    const start = line.indexOf('color-mix(', i);
    if (start === -1) break;
    let depth = 0;
    let end = start + 'color-mix('.length - 1;
    for (; end < line.length; end++) {
      if (line[end] === '(') depth++;
      else if (line[end] === ')') {
        depth--;
        if (depth === 0) break;
      }
    }
    spans.push(line.slice(start, end + 1));
    i = end + 1;
  }
  return spans;
}

function checkFile(path, originalLines, codeLines, violations, anchorDefs) {
  for (let idx = 0; idx < codeLines.length; idx++) {
    const line = codeLines[idx];

    // Rule 1: literal white/black inside color-mix(). The marker allowing an
    // exception lives in a comment, so its lookback searches the ORIGINAL
    // (comment-intact) lines, not the comment-stripped code lines.
    for (const span of extractColorMixSpans(line)) {
      if (LITERAL_MIX_RE.test(span)) {
        const windowStart = Math.max(0, idx - ANCHOR_EXCEPTION_LOOKBACK);
        const context = originalLines.slice(windowStart, idx + 1).join('\n');
        if (!context.includes(ANCHOR_EXCEPTION_MARKER)) {
          violations.push(
            `${path}:${idx + 1}: literal white/black inside color-mix(): ${span.trim()}`,
          );
        }
      }
    }

    // Rule 2: transition: all
    if (TRANSITION_ALL_RE.test(line)) {
      violations.push(`${path}:${idx + 1}: \`transition: all\` is banned: ${originalLines[idx].trim()}`);
    }

    // Rule 3: anchor definition sites
    for (const match of line.matchAll(ANCHOR_DEF_RE)) {
      const name = match[1];
      if (!anchorDefs.has(name)) anchorDefs.set(name, []);
      anchorDefs.get(name).push(`${path}:${idx + 1}`);
    }
  }
}

function main() {
  const files = collectCssFiles(SRC);
  const violations = [];
  const anchorDefs = new Map();

  for (const file of files) {
    const text = readFileSync(file, 'utf8');
    const originalLines = text.split('\n');
    const codeLines = stripComments(text).split('\n');
    const rel = file.slice(ROOT.length + 1);
    checkFile(rel, originalLines, codeLines, violations, anchorDefs);
  }

  for (const [name, sites] of anchorDefs) {
    if (sites.length > 1) {
      violations.push(
        `${name} is defined ${sites.length} times (exactly one definition site is required): ${sites.join(', ')}`,
      );
    }
  }

  const EXPECTED_ANCHORS = [
    '--anchor-paper',
    '--anchor-ink',
    '--anchor-blue',
    '--anchor-success',
    '--anchor-warn',
    '--anchor-danger',
  ];
  const missing = EXPECTED_ANCHORS.filter((a) => !anchorDefs.has(a));
  if (missing.length) {
    violations.push(`missing anchor definition(s): ${missing.join(', ')}`);
  }

  console.log(`checked ${files.length} CSS file(s) under src/`);
  console.log(`anchor definition sites: ${[...anchorDefs.entries()].map(([n, s]) => `${n} (${s.length})`).join(', ')}`);

  if (violations.length) {
    console.error(`\ntoken gate FAILED (${violations.length} violation(s)):`);
    for (const v of violations) console.error(`  - ${v}`);
    process.exit(1);
  }

  console.log('\ntoken gate PASSED: no literal white/black in color-mix(), no transition: all, one definition site per anchor.');
}

main();
