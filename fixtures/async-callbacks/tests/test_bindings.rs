/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
ubrn_macros::build_foreign_language_testcases! {
    // NOTE: Nitro is intentionally NOT in this matrix yet. The async
    // foreign-callback C++ ABI is implemented + verified (the emitted
    // `HybridAsyncParser.cpp` uses uniffi's foreign-future vtable signature
    // and compiles *and links* into a real `libnitro-async_callbacks.so`),
    // but this shared test script targets the jsi/napi/wasm *consumer* TS
    // surface — which exposes `ParserError` as a runtime value, a default
    // export with `initialize()`, and a plain (non-HybridObject) callback
    // class. The Nitro consumer surface vends none of those yet (errors are
    // `type`-only, there's no default export, and a `with_foreign` trait is
    // a `HybridObject` authored via `NitroModules.createHybridObject`), so
    // `tsc` rejects this script under Nitro. Wiring that surface parity is a
    // separate workstream from the callback ABI; flipping the flag here
    // would only add a red test, not exercise the ABI.
    "tests/bindings/test_async_callbacks.ts" => [Jsi, Wasm, Napi],
}
