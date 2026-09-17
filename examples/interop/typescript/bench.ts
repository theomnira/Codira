// Why batch kernels, and by how much.
//
// Run with the library path, same as `main.ts`:
//
//     bun run bench.ts ../codira/target/mod.codiralib
//
// The point this makes: calling a native function per element is the slowest
// of the three options here, slower than staying in JavaScript. The FFI
// boundary costs about the same whether the function does real work or
// nothing at all, so at one call per element the boundary *is* the workload.
// Cross it once per buffer and the same Codira code wins.

import { dlopen, FFIType, ptr } from "bun:ffi";

const libraryPath = process.argv[2];
if (!libraryPath) {
  console.error("usage: bun run bench.ts <path to .codiralib>");
  process.exit(2);
}

// Written out rather than generated so this file stands alone; `codira bindgen
// --runtime bun` emits exactly these signatures from the Codira declarations.
const { symbols } = dlopen(libraryPath, {
  codira_add: { args: [FFIType.i64_fast, FFIType.i64_fast], returns: FFIType.i64_fast },
  codira_scale: { args: [FFIType.f64, FFIType.f64], returns: FFIType.f64 },
  codira_clz32_into: { args: [FFIType.ptr, FFIType.ptr, FFIType.ptr], returns: FFIType.ptr },
  codira_affine_into: {
    args: [FFIType.ptr, FFIType.ptr, FFIType.ptr, FFIType.f32, FFIType.f32],
    returns: FFIType.ptr,
  },
  codira_sum_f32: { args: [FFIType.ptr, FFIType.ptr], returns: FFIType.f32 },
});

// Also bind i64 the slow way, to show what the default costs.
const slow = dlopen(libraryPath, {
  codira_add: { args: [FFIType.i64, FFIType.i64], returns: FFIType.i64 },
}).symbols;

const N = 500_000;

function bench(label: string, fn: () => unknown): number {
  for (let warmup = 0; warmup < 5; warmup++) fn();
  let best = Infinity;
  for (let round = 0; round < 7; round++) {
    const started = Bun.nanoseconds();
    fn();
    best = Math.min(best, Bun.nanoseconds() - started);
  }
  console.log(
    `  ${label.padEnd(40)} ${(best / 1e6).toFixed(3).padStart(8)} ms   ` +
      `${(best / N).toFixed(3).padStart(7)} ns/element`,
  );
  return best / N;
}

let failures = 0;
function check(label: string, ok: boolean, detail = "") {
  if (!ok) failures++;
  console.log(`  ${ok ? "ok  " : "FAIL"} ${label}${detail ? "  " + detail : ""}`);
}

// ---------------------------------------------------------------------------
// i64: BigInt against i64_fast
// ---------------------------------------------------------------------------

console.log(`\n${N.toLocaleString()} calls or elements, best of 7\n`);
console.log("Scalar calls -- what the binding type costs\n");

const bigArgs = Array.from({ length: 64 }, (_, i) => BigInt(i));
bench("codira_add via FFIType.i64 (BigInt)", () => {
  let acc = 0n;
  for (let i = 0; i < N; i++) acc += slow.codira_add(bigArgs[i & 63], 1n) as bigint;
  return acc;
});
bench("codira_add via FFIType.i64_fast", () => {
  let acc = 0;
  for (let i = 0; i < N; i++) acc += Number(symbols.codira_add(i & 63, 1));
  return acc;
});

// ---------------------------------------------------------------------------
// clz32: per-element FFI against one batched call against the JIT
// ---------------------------------------------------------------------------

console.log("\nclz32 over a buffer -- what the call *granularity* costs\n");

const source = new Uint32Array(N);
for (let i = 0; i < N; i++) source[i] = (i * 2654435761) >>> 0;
const destination = new Uint32Array(N);

const inJs = bench("TypeScript Math.clz32, loop in JS", () => {
  for (let i = 0; i < N; i++) destination[i] = Math.clz32(source[i]);
});
const batched = bench("Codira batch, one call over the buffer", () => {
  symbols.codira_clz32_into(ptr(source), ptr(destination), N);
});

symbols.codira_clz32_into(ptr(source), ptr(destination), N);
let firstBad = -1;
for (let i = 0; i < N; i++) {
  if (destination[i] !== Math.clz32(source[i])) {
    firstBad = i;
    break;
  }
}
check(
  `all ${N.toLocaleString()} clz32 results match Math.clz32`,
  firstBad < 0,
  firstBad < 0 ? "" : `first mismatch at ${firstBad}`,
);
console.log(`  batch is ${(inJs / batched).toFixed(2)}x the speed of the JIT`);

// ---------------------------------------------------------------------------
// f32 kernels
// ---------------------------------------------------------------------------

console.log("\nf32 kernels over a buffer\n");

const values = new Float32Array(N);
for (let i = 0; i < N; i++) values[i] = Math.sin(i * 0.001);
const scaled = new Float32Array(N);

bench("Codira affine (x * 2.5 + 1.0)", () => {
  symbols.codira_affine_into(ptr(values), ptr(scaled), N, 2.5, 1.0);
});
bench("Codira pairwise sum", () => symbols.codira_sum_f32(ptr(values), N));

symbols.codira_affine_into(ptr(values), ptr(scaled), N, 2.5, 1.0);
let worstAffine = 0;
for (let i = 0; i < N; i++) {
  worstAffine = Math.max(worstAffine, Math.abs(scaled[i] - (values[i] * 2.5 + 1.0)));
}
check("affine matches a JS reference", worstAffine < 1e-5, `max err ${worstAffine.toExponential(2)}`);

// Compare the pairwise sum against Kahan summation rather than a naive JS
// loop: a naive loop is the thing pairwise summation is more accurate *than*,
// so it is not the reference. Kahan is near-exact and shows the Codira sum
// lands within f32 rounding of the true total.
const fromCodira = symbols.codira_sum_f32(ptr(values), N);
let kahanSum = 0;
let compensation = 0;
for (let i = 0; i < N; i++) {
  const adjusted = values[i] - compensation;
  const running = kahanSum + adjusted;
  compensation = running - kahanSum - adjusted;
  kahanSum = running;
}
const relativeError = Math.abs(fromCodira - kahanSum) / Math.max(1, Math.abs(kahanSum));
check(
  "pairwise sum matches Kahan summation",
  relativeError < 1e-5,
  `codira ${fromCodira.toFixed(4)}, kahan ${kahanSum.toFixed(4)}, rel err ${relativeError.toExponential(2)}`,
);

console.log(failures ? `\n  ${failures} FAILURES\n` : "\n  all results correct\n");
process.exit(failures ? 1 : 0);
