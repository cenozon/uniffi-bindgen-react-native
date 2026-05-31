/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
ubrn_macros::build_foreign_language_testcases! {
    // The shared script targets the jsi/napi/wasm *consumer* TS surface —
    // which exposes `ParserError` as a runtime value, a default export with
    // `initialize()`, a plain (non-HybridObject) callback class, and the
    // `uniffiRustFutureHandleCount` counter. The Nitro consumer surface vends
    // none of those (errors are message-prefix + `<Error>_Tags.tagOf`, there's
    // no default export, a `with_foreign` trait is a `HybridObject` with a JS
    // -impl factory, and no future-handle counters), so `tsc` rejects this
    // script under Nitro. Nitro is exercised by a dedicated script below.
    "tests/bindings/test_async_callbacks.ts" => [Jsi, Wasm, Napi],
    // Nitro-flavor round-trip for the async foreign-callback path, written
    // against the Nitro consumer surface. HEADLINE: proves audit bug #4 — the
    // async-VOID `with_foreign` trait methods `delay` / `tryDelay` are bound
    // AND dispatched (previously accepted in the impl type but never bound, so
    // they threw "not implemented"). Each call is raced against a watchdog so
    // the alternate failure mode (audit bug #6 deadlock) surfaces as a bounded
    // failure rather than an unbounded hang.
    "tests/bindings/test_async_callbacks_nitro.ts" => [Nitro],
}
