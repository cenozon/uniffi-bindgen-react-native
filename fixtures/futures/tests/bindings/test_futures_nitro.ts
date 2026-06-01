/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
// Nitro round-trip for the JS-aborts-Rust-future path (parity item P1).
//
// To run:
//   cargo test -p uniffi-fixture-futures --test test_bindings -- nitro
//
// Targets the Nitro consumer surface only (no default-export `initialize()`, no
// `uniffiRustFutureHandleCount` — that counter is a JSI-internal of the TS poll
// loop and is never populated in the Nitro path, where the loop lives in C++).
// It proves the C++-side cancel plumbing (`future.hpp` CancelRegistry + the
// non-spec `__uniffiBeginAbortable()` / `__uniffiAbort(token)` hooks + the
// `namespace.ts` `signal.addEventListener` wiring) actually cancels an
// *in-flight* Rust future.
//
// Design note: we `await` the aborted `sleep` DIRECTLY (no watchdog race that
// would leave a long future pending and keep the Hermes event loop alive). If
// cancel works the aborted sleep settles promptly and the test finishes fast;
// if cancel were the old no-op the awaited sleep would not settle until its full
// duration and the harness `asyncTest` timeout would fail the test.

import { sleep } from "@/generated/futures";
import { asyncTest } from "@/asserts";
import "@/polyfills";

asyncTest(
  "(a) P1: aborting an in-flight sleep settles it promptly (cancel stops it)",
  async (t) => {
    const controller = new AbortController();
    const started = Date.now();
    const slow = sleep(3_000, { signal: controller.signal });

    // Let the future actually start, then abort it.
    await new Promise((r) => setTimeout(r, 100));
    controller.abort();

    let settledAs = "pending";
    try {
      await slow;
      settledAs = "resolved";
    } catch {
      settledAs = "rejected";
    }
    const elapsed = Date.now() - started;

    // A working cancel makes the aborted sleep settle WAY before its 3s natural
    // duration. uniffi maps a cancelled future's completion to a rejecting
    // error, so "rejected" is expected; the load-bearing proof is promptness.
    t.assertTrue(
      elapsed < 1_500,
      `aborted in-flight sleep should settle promptly (<1500ms); took ${elapsed}ms, settledAs=${settledAs}`,
    );
    t.assertEqual(settledAs, "rejected");
    t.end();
  },
);

asyncTest(
  "(b) P1: aborting BEFORE start rejects immediately",
  async (t) => {
    const controller = new AbortController();
    controller.abort();
    let rejected = false;
    try {
      await sleep(10, { signal: controller.signal });
    } catch {
      rejected = true;
    }
    t.assertTrue(rejected, "a pre-aborted async call should reject");
    t.end();
  },
);

asyncTest(
  "(c) P1: aborting AFTER settle is a clean no-op",
  async (t) => {
    const controller = new AbortController();
    const value = await sleep(10, { signal: controller.signal });
    // `sleep` returns true on completion.
    t.assertEqual(value, true);
    // Abort after the promise already settled: must not throw (the token was
    // deregistered on settle, so __uniffiAbort is a registry-miss no-op).
    controller.abort();
    t.assertTrue(true, "post-settle abort did not throw");
    t.end();
  },
);
