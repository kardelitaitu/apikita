// Types for the two Node built-ins the test suite uses.
//
// `@types/node` is deliberately not a dependency: the app itself uses no Node
// built-ins, and pulling a whole types package into the shared package.json for
// two modules would be the wrong trade. So the surface the suite actually calls
// is declared here by hand.
//
// This stub declares exactly the members the suite uses — no more. That is the
// closest a hand-written stub can come to not drifting: if a test ever calls an
// assert method or a `node:test` helper that is not declared below, `tsc` fails
// and the reader is forced back here to make a deliberate choice, instead of a
// wider stub quietly accepting whatever a new test happens to call. The
// signatures are transcribed from `@types/node` (Node 22) rather than guessed,
// so they pin the same contract the real types would.
//
// It also OVERRIDES a real `@types/node` if one is ever added. Both built-ins
// are declared with `export =`, so any extra `export` in the merged module makes
// TypeScript report TS2309 inside @types/node's own files — which `skipLibCheck`
// (on via astro/tsconfigs/strict) hides, making the override silent. So: if you
// add `@types/node`, DELETE THIS FILE in the same change; do not keep both.
//
//   cd website && npx --yes -p typescript@5.6 tsc --noEmit -p tsconfig.json

declare module 'node:test' {
  // Only `test` is used. A function declaration merges with (rather than
  // replaces) a real `@types/node` one, because declarations become overloads.
  export function test(name: string, fn: () => void | Promise<void>): void;
}

declare module 'node:assert/strict' {
  // Signatures mirror @types/node/assert.d.ts (Node 22).
  interface StrictAssert {
    ok(value: unknown, message?: string | Error): asserts value;
    equal(actual: unknown, expected: unknown, message?: string | Error): void;
    notEqual(actual: unknown, expected: unknown, message?: string | Error): void;
    deepEqual(actual: unknown, expected: unknown, message?: string | Error): void;
    match(value: string, regExp: RegExp, message?: string | Error): void;
    doesNotMatch(value: string, regExp: RegExp, message?: string | Error): void;
    throws(block: () => unknown, message?: string | Error): void;
    throws(block: () => unknown, error: AssertPredicate, message?: string | Error): void;
  }
  /** Transcribed from @types/node/assert.d.ts (Node 22). */
  type AssertPredicate =
    | RegExp
    | (new (...args: any[]) => object)
    | ((thrown: unknown) => boolean)
    | object
    | Error;
  const assert: StrictAssert;
  export default assert;
}
