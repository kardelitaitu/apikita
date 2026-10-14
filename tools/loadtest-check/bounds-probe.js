#!/usr/bin/env node
//
// bounds-probe — extracts a histogram boundary array from a source file and prints it as JSON.
//
// WHY THIS IS A FILE rather than a `node -e` inside check.sh, and the reason is a defect the gate
// had. The first version sliced the source at `(() => {` and cut at the first `)();`, which happens
// to work only because of how the declaration is currently formatted. MEASURED: it produced
// `eval("<body>")()` with the closing brace missing — "SyntaxError: Unexpected end of input" — and
// because its stderr was piped to /dev/null the gate reported only "could not read the boundary
// arrays", naming the comparison rather than the extraction.
//
// The fix is to stop guessing at the text and BALANCE BRACES: find the `(() =>` that opens the
// initialiser, walk forward counting `{` and `}`, and take the substring to the matching close. That
// is exactly the technique `server/src/doc_claims.rs` records as necessary in its own header —
// "Enforcing this properly needs real brace matching, which is a parser rather than a matcher" — and
// it is needed here for the same reason: a matcher that reads a PREFIX of its input reports the wrong
// thing about the rest of it.
//
// Usage: node tools/loadtest-check/bounds-probe.js <file> <DECLARATION_NAME>
// Exit: 0 prints the array as JSON, 1 the declaration or its initialiser could not be found,
//       3 wrong arguments.
'use strict';

const fs = require('node:fs');

const [file, name] = process.argv.slice(2);
if (!file || !name) {
  console.error('bounds-probe: usage: bounds-probe.js <file> <DECLARATION_NAME>');
  process.exit(3);
}

const src = fs.readFileSync(file, 'utf8');

// The declaration. Anchored on `const <NAME> =` so a mention of the name in a comment or a call
// site is not mistaken for the definition.
const declAt = src.indexOf(`const ${name} =`);
if (declAt === -1) {
  console.error(`bounds-probe: ${file} has no \`const ${name} =\` declaration`);
  process.exit(1);
}

// The arrow-function initialiser that follows it.
const arrowAt = src.indexOf('(() =>', declAt);
if (arrowAt === -1) {
  console.error(`bounds-probe: \`const ${name} =\` is not initialised by an arrow function in ${file}`);
  process.exit(1);
}

// BALANCE THE BRACES from the first `{` of the arrow body to its match. String literals are skipped
// so a brace inside a template or a quoted string cannot unbalance the walk — the arrays here hold
// numbers, but the next edit might hold a string, and a walk that broke on one would report the
// wrong end of the region rather than fail.
const bodyStart = src.indexOf('{', arrowAt);
if (bodyStart === -1) {
  console.error(`bounds-probe: the arrow function for ${name} has no body`);
  process.exit(1);
}
let depth = 0;
let end = -1;
let quote = null;
for (let i = bodyStart; i < src.length; i += 1) {
  const c = src[i];
  if (quote !== null) {
    if (c === '\\') { i += 1; continue; }
    if (c === quote) quote = null;
    continue;
  }
  if (c === '"' || c === "'" || c === '`') { quote = c; continue; }
  if (c === '/' && src[i + 1] === '/') { while (i < src.length && src[i] !== '\n') i += 1; continue; }
  if (c === '{') depth += 1;
  else if (c === '}') {
    depth -= 1;
    if (depth === 0) { end = i; break; }
  }
}
if (end === -1) {
  console.error(`bounds-probe: could not find the matching brace for ${name}'s initialiser in ${file}`);
  process.exit(1);
}

// The invocation. TWO PIECES ARE TAKEN, and the first version took only one — which is the bug that
// made this a file rather than a `node -e`. The declaration initialiser runs `(() => { ... })()`, so
// the balanced region ends at the arrow body's closing `}` and the `)` that closes the parameter
// list sits AFTER it. Slicing to the brace alone produced `(() => {...})()` with a `(` unclosed —
// "Unexpected end of input" — and the fix is to include the closing paren.
const closer = src.indexOf(')', end + 1);
if (closer === -1) {
  console.error(`bounds-probe: no closing paren after ${name}'s initialiser body in ${file}`);
  process.exit(1);
}
const expression = `(${src.slice(arrowAt, closer + 1)})()`;
let value;
try {
  // eslint-disable-next-line no-eval
  value = eval(expression);
} catch (err) {
  console.error(`bounds-probe: evaluating ${name}'s initialiser failed: ${err.message}`);
  process.exit(1);
}

if (!Array.isArray(value) || value.length === 0) {
  console.error(`bounds-probe: ${name} did not evaluate to a non-empty array (got ${typeof value})`);
  process.exit(1);
}
// A boundary array must be strictly increasing, or a sample can fall in two buckets or none — which
// would mis-place the tail without changing the total, the exact silent failure the comparison in
// check.sh exists to catch. Asserted here so a malformed array fails at its own declaration.
for (let i = 1; i < value.length; i += 1) {
  if (!(value[i] > value[i - 1])) {
    console.error(`bounds-probe: ${name} is not strictly increasing at index ${i} (${value[i - 1]} -> ${value[i]})`);
    process.exit(1);
  }
}
if (!Number.isFinite(value[value.length - 1])) {
  console.error(`bounds-probe: ${name}'s last boundary is not finite (${value[value.length - 1]}); JSON would carry it as null and every sample above the last real bucket would be lost`);
  process.exit(1);
}

process.stdout.write(JSON.stringify(value));
