// Calls a Codira library from TypeScript through Bun's built-in FFI.
//
// No node-gyp, no native addon, no npm dependency: the library exports plain
// C symbols, and `bun:ffi` can call those directly. Deno's `Deno.dlopen` has
// the same shape if you prefer it.
//
//     bun run main.ts ../codira/target/mod.codiralib

import { dlopen, FFIType, suffix } from "bun:ffi";

const libraryPath = process.argv[2];
if (!libraryPath) {
  console.error(`usage: bun run main.ts <path to .codiralib>  (platform suffix: .${suffix})`);
  process.exit(2);
}

// The signatures are the contract `@export("C")` declared on the Codira side.
// Bun needs them explicitly -- there is no metadata in a C symbol to infer
// them from, which is exactly why they have to be written down here.
const { symbols } = dlopen(libraryPath, {
  codira_add: {
    args: [FFIType.i64, FFIType.i64],
    returns: FFIType.i64,
  },
  codira_scale: {
    args: [FFIType.f64, FFIType.f64],
    returns: FFIType.f64,
  },
  codira_sum_of_squares: {
    args: [FFIType.f64, FFIType.f64],
    returns: FFIType.f64,
  },
  codira_length_squared: {
    args: [FFIType.f64, FFIType.f64],
    returns: FFIType.f64,
  },
  codira_scaled_length_squared: {
    args: [FFIType.f64, FFIType.f64, FFIType.f64],
    returns: FFIType.f64,
  },
  codira_div: {
    args: [FFIType.i64, FFIType.i64],
    returns: FFIType.i64,
  },
  codira_mod: {
    args: [FFIType.i64, FFIType.i64],
    returns: FFIType.i64,
  },
});

type Check = readonly [label: string, actual: number | bigint, expected: number | bigint];

const checks: Check[] = [
  ["codira_add(20, 22)", symbols.codira_add(20n, 22n), 42n],
  ["codira_scale(1.5, 4.0)", symbols.codira_scale(1.5, 4.0), 6.0],
  ["codira_sum_of_squares(3, 4)", symbols.codira_sum_of_squares(3.0, 4.0), 25.0],
  ["codira_length_squared(3, 4)", symbols.codira_length_squared(3.0, 4.0), 25.0],
  [
    "codira_scaled_length_squared(3, 4, 2)",
    symbols.codira_scaled_length_squared(3.0, 4.0, 2.0),
    100.0,
  ],
  ["codira_div(47, 5)", symbols.codira_div(47n, 5n), 9n],
  ["codira_mod(47, 5)", symbols.codira_mod(47n, 5n), 2n],
];

let failures = 0;
for (const [label, actual, expected] of checks) {
  // `==` rather than `===` on purpose: an i64 comes back as a BigInt while
  // the expected value may be written as a number, and comparing the
  // *values* is what this is checking.
  const ok = actual == expected;
  if (!ok) failures++;
  console.log(
    `  ${ok ? "ok  " : "FAIL"} ${label} = ${actual}` +
      (ok ? "" : ` (expected ${expected})`),
  );
}

process.exit(failures ? 1 : 0);
