// The only Node built-ins the test suite uses.
//
// The app itself uses none, so `@types/node` is not a dependency and pulling one
// into the shared package.json for two modules would be the wrong trade. These
// declarations cover exactly the surface `rate-limit.test.ts` calls; if the
// suite ever needs more, add it here or take the dependency deliberately.

declare module 'node:test' {
  export function test(name: string, fn: () => void | Promise<void>): void;
}

declare module 'node:assert/strict' {
  interface StrictAssert {
    equal(actual: unknown, expected: unknown, message?: string): void;
    deepEqual(actual: unknown, expected: unknown, message?: string): void;
    ok(value: unknown, message?: string): void;
  }
  const assert: StrictAssert;
  export default assert;
}
