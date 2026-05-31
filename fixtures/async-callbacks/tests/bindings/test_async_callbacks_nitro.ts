/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
// Nitro-flavor round-trip for the async foreign-callback path.
//
// The shared `test_async_callbacks.ts` script is jsi/napi/wasm-only: it uses
// a default-export `initialize()`, field-typed `ParserError.<Variant>()`
// runtime constructors, and `uniffiRustFutureHandleCount` — none of which the
// Nitro consumer surface vends (locked decisions: no default export, errors
// are message-prefix + `<Error>_Tags.tagOf`, no future-handle counters). This
// script targets the *Nitro* surface instead:
//   * `AsyncParser(impl)` JS-impl factory (the callback interface is a
//     HybridObject under Nitro, so a bare object can't be passed; the factory
//     wraps the impl via the C++ `setJsImpl` hook).
//   * `ParserError_Tags.tagOf(e)` / `is<Variant>(e)` for discriminating a
//     thrown error by its `ParserError::<Variant>` message prefix.
//
// HEADLINE: it proves audit bug #4 — the async-VOID `with_foreign` trait
// methods `delay` / `tryDelay` are bound into the impl type AND dispatched.
// Before the fix they were accepted in the generated impl type but never
// bound, so `delayUsingTrait` / `tryDelayUsingTrait` threw "not implemented"
// at dispatch time. Each call is raced against a watchdog so the regression's
// alternate failure mode (audit bug #6, an async-void deadlock that never
// settles) shows up as a bounded test failure rather than an unbounded hang.

import {
  asStringUsingTrait,
  delayUsingTrait,
  ParserError_Tags,
  tryDelayUsingTrait,
  tryFromStringUsingTrait,
} from "@/generated/async_callbacks";
import type { AsyncParser } from "@/generated/async_callbacks";
import * as asyncCallbacksModule from "@/generated/async_callbacks";
import { asyncTest } from "@/asserts";

// ---------------------------------------------------------------------------
// Flavor-aware callback construction.
//
// Under Nitro, `AsyncParser` is exported as BOTH a type and a runtime factory
// (declaration merging). On jsi/napi/wasm there is no factory — a plain object
// implementing the methods is passed directly. `makeAsyncParser` papers over
// the difference so this script could in principle run on any backend; under
// Nitro it always takes the factory branch.
// ---------------------------------------------------------------------------
interface AsyncParserImpl {
  asString(delayMs: number, value: number): Promise<string>;
  tryFromString(delayMs: number, value: string): Promise<number>;
  delay(delayMs: number): Promise<void>;
  tryDelay(delayMs: string): Promise<void>;
}

function makeAsyncParser(impl: AsyncParserImpl): AsyncParser {
  const factory = (asyncCallbacksModule as Record<string, unknown>).AsyncParser;
  if (typeof factory === "function") {
    return (factory as (i: AsyncParserImpl) => AsyncParser)(impl);
  }
  return impl as unknown as AsyncParser;
}

function delayPromise(delayMs: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, delayMs));
}

// Race a promise against a bounded watchdog so a deadlock (audit bug #6 — the
// async-void path never settling) surfaces as a thrown timeout rather than an
// unbounded hang that the outer `asyncTest` timeout would only catch much
// later.
async function withWatchdog<T>(
  label: string,
  p: Promise<T>,
  ms: number,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const watchdog = new Promise<never>((_resolve, reject) => {
    timer = setTimeout(
      () => reject(new Error(`WATCHDOG: '${label}' did not settle within ${ms}ms (deadlock?)`)),
      ms,
    );
  });
  try {
    return await Promise.race([p, watchdog]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

const WATCHDOG_MS = 8_000;
const TEST_TIMEOUT_MS = 30_000;

// A foreign `TsAsyncParser` implementing all four trait methods. The two
// async-VALUE methods (`asString`, `tryFromString`) derive their result from
// the input so a correct round-trip is distinguishable from a no-op; the two
// async-VOID methods (`delay`, `tryDelay`) increment `completedDelays` so the
// test can prove they actually ran (not just resolved).
class TsAsyncParser implements AsyncParserImpl {
  completedDelays = 0;

  async asString(delayMs: number, value: number): Promise<string> {
    await this.doDelay(delayMs);
    return value.toString();
  }

  async tryFromString(delayMs: number, value: string): Promise<number> {
    const v = this.parseInt(value);
    await this.doDelay(delayMs);
    return v;
  }

  async delay(delayMs: number): Promise<void> {
    await this.doDelay(delayMs);
  }

  async tryDelay(delayMs: string): Promise<void> {
    await this.doDelay(this.parseInt(delayMs));
  }

  private async doDelay(ms: number): Promise<void> {
    await delayPromise(ms);
    this.completedDelays += 1;
  }

  private parseInt(value: string): number {
    const num = Number.parseInt(value, 10);
    if (Number.isNaN(num)) {
      // The Rust `ParserError::NotAnInt` round-trips back to JS as a thrown
      // error whose message carries the `ParserError::NotAnInt` prefix; here
      // we just signal failure so Rust's `try_*` returns the Err.
      throw new Error("ParserError::NotAnInt(not an int)");
    }
    return num;
  }
}

(async () => {
  // -------------------------------------------------------------------------
  // 1. Async-VALUE trait methods round-trip (sanity: the bridge + factory
  //    wiring is alive before we get to the headline void-method test).
  // -------------------------------------------------------------------------
  await asyncTest(
    "nitro async-callbacks: async-value methods round-trip",
    async (t) => {
      const traitObj = new TsAsyncParser();

      const s = await withWatchdog(
        "asStringUsingTrait",
        asStringUsingTrait(makeAsyncParser(traitObj), 1, 42),
        WATCHDOG_MS,
      );
      t.assertEqual(s, "42", "asString should round-trip 42 -> '42'");

      const n = await withWatchdog(
        "tryFromStringUsingTrait",
        tryFromStringUsingTrait(makeAsyncParser(traitObj), 1, "42"),
        WATCHDOG_MS,
      );
      t.assertEqual(n, 42, "tryFromString should round-trip '42' -> 42");

      t.end();
    },
    TEST_TIMEOUT_MS,
  );

  // -------------------------------------------------------------------------
  // 2. HEADLINE (audit bug #4): async-VOID trait methods `delay` / `tryDelay`
  //    must be bound AND dispatched. Each must RESOLVE under the watchdog
  //    (no "not implemented", no #6 deadlock), and the impl's side effect
  //    (`completedDelays`) must observably increment — proving the JS method
  //    actually ran rather than the Promise resolving vacuously.
  // -------------------------------------------------------------------------
  await asyncTest(
    "nitro async-callbacks: async-void delay/tryDelay bound + dispatched (bug #4)",
    async (t) => {
      const traitObj = new TsAsyncParser();
      const cb = makeAsyncParser(traitObj);

      const before = traitObj.completedDelays;

      // `delay` — async fn -> () : ForeignFutureCallback<void>.
      await withWatchdog(
        "delayUsingTrait",
        delayUsingTrait(cb, 1),
        WATCHDOG_MS,
      );

      // `tryDelay` — async fn -> Result<(), ParserError> : the fallible
      // void path. A valid numeric string must resolve (Ok(())).
      await withWatchdog(
        "tryDelayUsingTrait",
        tryDelayUsingTrait(cb, "1"),
        WATCHDOG_MS,
      );

      t.assertEqual(
        traitObj.completedDelays,
        before + 2,
        "both async-void methods must have actually run (completedDelays += 2)",
      );

      t.end();
    },
    TEST_TIMEOUT_MS,
  );

  // -------------------------------------------------------------------------
  // 3. Fallible async-void error path: `tryDelay` with a non-numeric string
  //    makes the JS impl throw. A JS callback reject reaches C++ only as a
  //    string-only `jsi::JSError` (no typed-error wire bytes survive — locked
  //    decision E1), so the async-foreign-future trampoline reports it on the
  //    `code=2` "unexpected" channel. uniffi then runs the author's
  //    `From<UnexpectedUniFFICallbackError> for ParserError`, which yields
  //    `ParserError::UnexpectedError`. The call must REJECT (still bounded —
  //    proving error propagation on the void path is wired, not deadlocked)
  //    and the thrown error must be discriminable via `ParserError_Tags`.
  // -------------------------------------------------------------------------
  await asyncTest(
    "nitro async-callbacks: fallible async-void rejects + is discriminable",
    async (t) => {
      const traitObj = new TsAsyncParser();
      const cb = makeAsyncParser(traitObj);

      let threw = false;
      try {
        await withWatchdog(
          "tryDelayUsingTrait(non-numeric)",
          tryDelayUsingTrait(cb, "not-a-number"),
          WATCHDOG_MS,
        );
      } catch (e) {
        threw = true;
        // The error must carry a discriminable `ParserError` tag, recovered
        // Nitro-side via the `<Error>_Tags.tagOf` message-prefix helper rather
        // than a field-typed payload. A JS-callback reject maps through
        // `From<UnexpectedUniFFICallbackError>` to `ParserError::UnexpectedError`.
        const tag = ParserError_Tags.tagOf(e);
        t.assertTrue(
          tag === ParserError_Tags.UnexpectedError,
          `thrown error should be ParserError::UnexpectedError, got tag=${String(tag)} (e=${String(e)})`,
        );
      }
      t.assertTrue(threw, "fallible async-void must reject on a bad input");

      t.end();
    },
    TEST_TIMEOUT_MS,
  );
})();
