/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
// Standalone runtime probe for the ASYNC foreign-callback path under Nitro.
// Races a single `invokeAsyncCallback` against a watchdog so a deadlock shows
// up as a bounded "TIMED OUT" line rather than an unbounded hang. A correct
// round-trip prints the lifted value (expected 43 for x=21).

import {
  invokeAsyncCallback,
} from "@/generated/uniffi_benchmark";
import type { BenchCallback } from "@/generated/uniffi_benchmark";
import * as benchmarkModule from "@/generated/uniffi_benchmark";
import { asyncTest } from "@/asserts";

(
  benchmarkModule as unknown as { default?: { initialize?: () => void } }
).default?.initialize?.();

interface BenchCallbackImpl {
  runSync(x: number): number;
  runAsync(x: number): Promise<number>;
}

function makeBenchCallback(impl: BenchCallbackImpl): BenchCallback {
  const factory = (benchmarkModule as Record<string, unknown>).BenchCallback;
  if (typeof factory === "function") {
    return (factory as (i: BenchCallbackImpl) => BenchCallback)(impl);
  }
  return impl as unknown as BenchCallback;
}

const cbImpl: BenchCallbackImpl = {
  runSync(x: number): number {
    return x * 2 + 1;
  },
  async runAsync(x: number): Promise<number> {
    return x * 2 + 1;
  },
};

(async () => {
  await asyncTest(
    "probe: invokeAsyncCallback (Rust -> JS async callback dispatch)",
    async (t) => {
      console.log(
        "\n--- PROBE invokeAsyncCallback: async foreign-callback crossing ---",
      );
      const cb = makeBenchCallback(cbImpl);

      let timer: ReturnType<typeof setTimeout> | undefined;
      const watchdog = new Promise<{ kind: "timeout" }>((resolve) => {
        timer = setTimeout(() => resolve({ kind: "timeout" }), 8000);
      });

      const call = (async () => {
        try {
          const v = await invokeAsyncCallback(cb, 21);
          return { kind: "value" as const, v };
        } catch (e) {
          return { kind: "error" as const, e: String(e) };
        }
      })();

      const result = await Promise.race([call, watchdog]);
      if (timer) clearTimeout(timer);

      if (result.kind === "timeout") {
        console.log("  RESULT: TIMED OUT (deadlock / never settled)");
      } else if (result.kind === "error") {
        console.log(`  RESULT: THREW -> ${result.e}`);
      } else {
        console.log(
          `  RESULT: resolved -> ${result.v} (expected 43; correct=${result.v === 43})`,
        );
      }
      t.end();
    },
    30_000,
  );
})();
