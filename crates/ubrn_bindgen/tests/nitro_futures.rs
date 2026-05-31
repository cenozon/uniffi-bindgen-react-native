/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
//! Nitro emission against the `futures` fixture — exercises the async +
//! typed-error code paths that the `arithmetic` fixture leaves
//! untouched. The fixture has:
//!
//!   * `async fn always_ready() -> bool` — async, no throw, no args
//!   * `async fn say_after(ms: u16, who: String) -> String` — async,
//!     RustBuffer return
//!   * `async fn fallible_me(do_fail: bool) -> Result<u8, MyError>`  —
//!     async, throws
//!   * `enum MyError { Foo }` — flat error
//!   * `enum AsyncError { Timeout }` — flat error
//!   * `async fn sleep_no_return(ms: u16)` — async, void return
//!
//! That covers all four async × throws combinations plus the new
//! `Promise<T>` / `Promise<void>` return signatures the spec emits.

use std::path::PathBuf;

use camino::Utf8PathBuf;

use ubrn_bindgen::{AbiFlavor, BindingsArgs, OutputArgs, SourceArgs, SwitchArgs};

fn find_built_cdylib(crate_lib_name: &str) -> Option<Utf8PathBuf> {
    let candidates = [
        format!("target/debug/lib{crate_lib_name}.so"),
        format!("target/debug/deps/lib{crate_lib_name}.so"),
        format!("target/release/lib{crate_lib_name}.so"),
        format!("target/release/deps/lib{crate_lib_name}.so"),
    ];
    let cargo_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = cargo_manifest.parent()?.parent()?;
    for c in candidates {
        let p = workspace_root.join(c);
        if p.exists() {
            return Utf8PathBuf::from_path_buf(p).ok();
        }
    }
    None
}

#[test]
fn nitro_emit_against_futures_cdylib() {
    let Some(lib) = find_built_cdylib("uniffi_futures") else {
        eprintln!(
            "skipping: libuniffi_futures.so not found — build `cargo build -p uniffi-fixture-futures` first"
        );
        return;
    };

    let out = tempfile::Builder::new()
        .prefix("ubrn-nitro-futures-emit-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("tempdir")
        .keep();
    let ts_dir = Utf8PathBuf::from_path_buf(out.join("ts")).unwrap();
    let cpp_dir = Utf8PathBuf::from_path_buf(out.join("cpp")).unwrap();
    std::fs::create_dir_all(&ts_dir).unwrap();
    std::fs::create_dir_all(&cpp_dir).unwrap();
    eprintln!("nitro futures emit -> {}", out.display());

    let switches = SwitchArgs {
        flavor: AbiFlavor::Nitro,
    };
    let source = SourceArgs::library(&lib);
    let output = OutputArgs::new(&ts_dir, &cpp_dir, /* no_format */ true);
    let args = BindingsArgs::new(switches, source, output);

    let _ = args.run(None).expect("nitro emission against futures");

    // --- TS spec --------------------------------------------------------
    let spec_path = ts_dir.join("Futures.nitro.ts");
    assert!(spec_path.exists(), "spec at {spec_path}");
    let spec = std::fs::read_to_string(&spec_path).unwrap();

    // Async fn always_ready() -> bool surfaces as `alwaysReady(): Promise<boolean>`.
    assert!(
        spec.contains("alwaysReady(): Promise<boolean>"),
        "spec should declare Promise<boolean> for async fn always_ready, got:\n{spec}"
    );
    // sleep_no_return is `async fn(...) -> ()` — Promise<void>.
    assert!(
        spec.contains("sleepNoReturn"),
        "spec should declare sleepNoReturn"
    );
    assert!(
        spec.contains("Promise<void>"),
        "spec should have a Promise<void> for sleep_no_return"
    );
    // Errors no longer emit a dead type-only `<Name>Variant` alias in the spec
    // (audit bug #8 dropped it). The runtime discriminable surface lives in the
    // consumer module: a `<Err>_Tags` string enum + a `<Err>` helper with
    // `tagOf` / per-variant `is<Variant>` parsers.
    assert!(
        !spec.contains("MyErrorVariant") && !spec.contains("AsyncErrorVariant"),
        "spec must not emit the dead `<Name>Variant` type alias; got:\n{spec}"
    );
    let reexport_path = ts_dir.join("futures.ts");
    assert!(reexport_path.exists(), "consumer module at {reexport_path}");
    let reexport = std::fs::read_to_string(&reexport_path).unwrap();
    assert!(
        reexport.contains("export enum MyError_Tags")
            && reexport.contains("export namespace MyError_Tags"),
        "consumer module should declare the MyError runtime error helper; got:\n{reexport}"
    );
    assert!(
        reexport.contains("export enum AsyncError_Tags")
            && reexport.contains("export namespace AsyncError_Tags"),
        "consumer module should declare the AsyncError runtime error helper; got:\n{reexport}"
    );

    // --- C++ namespace API impl ----------------------------------------
    let api_cpp_path = cpp_dir.join("HybridFuturesApi.cpp");
    assert!(api_cpp_path.exists(), "C++ impl at {api_cpp_path}");
    let api_cpp = std::fs::read_to_string(&api_cpp_path).unwrap();

    // Async methods return a `Promise<bool>` driven by the event-driven
    // rust_future poll loop (`drive_rust_future_async`, asserted below).
    assert!(
        api_cpp.contains("Promise<bool>>"),
        "async fn always_ready should return Promise<bool>, got:\n{api_cpp}"
    );
    // The rust_future poll loop helper must be invoked.
    assert!(
        api_cpp.contains("drive_rust_future"),
        "async fn body should drive the rust_future poll loop"
    );
    // The per-return-type poll/complete/free symbols must be referenced.
    assert!(
        api_cpp.contains("ffi_uniffi_futures_rust_future_poll_i8"),
        "always_ready (bool returns i8) must reference the i8 poll symbol"
    );
    assert!(
        api_cpp.contains("ffi_uniffi_futures_rust_future_complete_i8"),
        "always_ready must reference the i8 complete symbol"
    );
    assert!(
        api_cpp.contains("ffi_uniffi_futures_rust_future_free_i8"),
        "always_ready must reference the i8 free symbol"
    );
    // Async + throws: fallible_me catches UniffiTypedError and rethrows the
    // decoded exception. The exception class strips the trailing `Error` and
    // appends `Exception` (audit bug #28), so `MyError` → `MyException` and the
    // decoder is `lift_MyException` (NOT the old doubled `lift_MyErrorError`).
    assert!(
        api_cpp.contains("UniffiTypedError"),
        "async throws path must catch UniffiTypedError"
    );
    assert!(
        api_cpp.contains("lift_MyException"),
        "fallible_me must decode via lift_MyException"
    );

    // --- C++ codecs header ---------------------------------------------
    let codecs_path = cpp_dir.join("futures_codecs.hpp");
    assert!(codecs_path.exists(), "codecs at {codecs_path}");
    let codecs = std::fs::read_to_string(&codecs_path).unwrap();
    assert!(
        codecs.contains("class MyException"),
        "codecs must declare the MyException exception class"
    );
    assert!(
        codecs.contains("class AsyncException"),
        "codecs must declare the AsyncException exception class"
    );
    assert!(
        codecs.contains("MyException lift_MyException(RustBuffer"),
        "codecs must define lift_MyException"
    );
    assert!(
        codecs.contains("Kind::Foo"),
        "codecs must emit a Foo variant for MyError"
    );

    // Mirror the emit dir to a stable target/tmp location so a human
    // can inspect the generated files after the test passes. Cleanup
    // is best-effort; failed assertions earlier in the test leave the
    // original tempdir behind for forensics either way.
    let stable = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ubrn-nitro-futures-last");
    let _ = std::fs::remove_dir_all(&stable);
    let _ = copy_dir_recursive(&out, &stable);
    std::fs::remove_dir_all(out).ok();
}

/// Recursive directory copy. The emission is tiny (a handful of files),
/// so reaching for `walkdir` would be overkill.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
