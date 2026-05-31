/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
ubrn_macros::build_foreign_language_testcases! {
    "tests/bindings/test_coverall.ts" => [Jsi, Wasm, Napi],
    // Nitro-specific round-trip script. The JSI script above leans on
    // JSI-only surface (field-typed error payloads, `uniffiDestroy`,
    // default-export `initialize()`, …) that the Nitro backend does not
    // emit, so the Nitro coverage lives in its own script that only
    // exercises the Nitro-supported surface.
    "tests/bindings/test_coverall_nitro.ts" => [Nitro],
}
