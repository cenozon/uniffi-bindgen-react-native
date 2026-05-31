/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
// Nitro-specific round-trip test for the `coverall` fixture.
//
// To run:
//   cargo build -p uniffi-fixture-coverall
//   cargo test -p uniffi-fixture-coverall --test test_bindings -- nitro
//
// This script deliberately does NOT reuse the JSI test (`test_coverall.ts`):
// that one leans on JSI-only surface (field-typed error payloads such as
// `new ComplexError.OsError({...})`, `uniffiDestroy()`, a default-export
// `coverall.initialize()`, etc.) that the Nitro backend does not (and by the
// locked decisions cannot) emit. Here we exercise ONLY the surface the Nitro
// emitter actually ships, against the real native module:
//
//   (a) `new Patch(color)` constructs + `getColor()` round-trips.
//   (b) `new Coveralls(name)` constructs + `getName()` round-trips.
//   (c) `Coveralls.fallibleNew(name, false)` returns; `(name, true)` throws;
//       `Coveralls.panickingNew(...)` throws (panic surfaced as an error).
//   (d) `FalliblePatch.secondary()` and `new FalliblePatch()` both surface
//       their Rust-side `CoverallError::TooManyHoles` throw.
//   (e) `getDict3(numericKey, value)` returns a real JS `Map` whose numeric
//       key resolves via `.get(numericKey)`.
//   (f) a record (`ErrorDict`) built with its optional fields OMITTED
//       round-trips through `getErrorDict`.
//   (g) a thrown typed error is discriminable via the emitted
//       `<Error>_Tags.tagOf` / `is<Variant>` message-prefix API (NOT a
//       field-typed payload).

import {
  Coveralls,
  Patch,
  FalliblePatch,
  getErrorDict,
  throwRootError,
  Color,
  type ErrorDict,
  CoverallError_Tags,
  RootError_Tags,
  ComplexError_Tags,
} from "@/generated/coverall";
import { test } from "@/asserts";
import "@/polyfills";

test("(a) new Patch(color) constructs and getColor() round-trips", (t) => {
  const patch = new Patch(Color.Red);
  t.assertTrue(Patch.instanceOf(patch), "constructed value should be a Patch");
  t.assertEqual(patch.getColor(), Color.Red);

  const blue = new Patch(Color.Blue);
  t.assertEqual(blue.getColor(), Color.Blue);
});

test("(b) new Coveralls(name) constructs and getName() round-trips", (t) => {
  const c = new Coveralls("nitro_ctor");
  t.assertTrue(
    Coveralls.instanceOf(c),
    "constructed value should be a Coveralls",
  );
  t.assertEqual(c.getName(), "nitro_ctor");
});

test("(c) Coveralls.fallibleNew / panickingNew", (t) => {
  // Non-failing path returns a live object.
  const ok = Coveralls.fallibleNew("fallible_ok", false);
  t.assertTrue(Coveralls.instanceOf(ok));
  t.assertEqual(ok.getName(), "fallible_ok");

  // Failing path throws the typed CoverallError::TooManyHoles.
  t.assertThrows(
    (e) => CoverallError_Tags.isTooManyHoles(e),
    () => Coveralls.fallibleNew("fallible_fail", true),
  );

  // panicking_new always panics; the panic surfaces JS-side as a thrown error.
  t.assertThrows(
    (e) => e instanceof Error,
    () => Coveralls.panickingNew("expected panic in ctor"),
  );
});

test("(d) FalliblePatch.secondary() + new FalliblePatch() surface their throw", (t) => {
  // Rust `FalliblePatch::secondary()` always returns Err(TooManyHoles).
  t.assertThrows(
    (e) => CoverallError_Tags.isTooManyHoles(e),
    () => FalliblePatch.secondary(),
  );
  // Rust `FalliblePatch::new()` (the fallible primary ctor) likewise throws.
  t.assertThrows(
    (e) => CoverallError_Tags.isTooManyHoles(e),
    () => new FalliblePatch(),
  );
});

test("(e) getDict3 returns a real JS Map with a numeric key", (t) => {
  const c = new Coveralls("dict3");
  const m = c.getDict3(42, BigInt("7"));
  t.assertTrue(m instanceof Map, () => `expected a Map, got ${typeof m}`);
  // The key crossed as a JS number (u32 -> number), so `.get(42)` resolves.
  t.assertEqual(m.get(42), BigInt("7"));
  t.assertEqual(m.size, 1);
});

test("(f) ErrorDict round-trips with its optional fields OMITTED", (t) => {
  // `complexError` and `rootError` are `?`-optional in the Nitro record type,
  // so the literal can legally omit them.
  const input: ErrorDict = { errors: [] };
  const out = getErrorDict(input);
  t.assertNull(out.complexError);
  t.assertNull(out.rootError);
  t.assertEqual(out.errors.length, 0);
});

test("(g) thrown typed error is discriminable via <Error>_Tags", (t) => {
  // `throwRootError()` throws `RootError::Complex { ComplexError::OsError }`.
  // The Nitro surface recovers the variant tag from the error message prefix.
  t.assertThrows(
    (e) => RootError_Tags.tagOf(e) === RootError_Tags.Complex,
    () => throwRootError(),
  );
  t.assertThrows((e) => RootError_Tags.isComplex(e), () => throwRootError());

  // It is NOT the other variant.
  t.assertThrows(
    (e) => !RootError_Tags.isOther(e),
    () => throwRootError(),
  );

  // And the sync method path: `maybeThrow(true)` throws CoverallError.
  const c = new Coveralls("typed_errors");
  t.assertThrows(
    (e) => CoverallError_Tags.isTooManyHoles(e),
    () => c.maybeThrow(true),
  );
  // `maybeThrowComplex(1)` throws ComplexError::OsError.
  t.assertThrows(
    (e) => ComplexError_Tags.isOsError(e),
    () => c.maybeThrowComplex(1),
  );
});
