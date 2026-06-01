/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
ubrn_macros::build_foreign_language_testcases! {
    "tests/bindings/test_futures.ts" => [Jsi, Napi],
    // Nitro-only round-trip for the JS-aborts-Rust-future path (parity item P1).
    "tests/bindings/test_futures_nitro.ts" => [Nitro],
}
