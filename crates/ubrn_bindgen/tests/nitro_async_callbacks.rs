// SPDX-License-Identifier: MPL-2.0
//
// End-to-end test: drive `BindingsArgs::run` with `AbiFlavor::Nitro`
// against the `async-callbacks` fixture crate's freshly-built cdylib and
// verify that an `async fn` method on a `#[uniffi::export(with_foreign)]`
// trait surfaces in the `.nitro.ts` spec as a `Promise<T>`-returning
// HybridObject method — NOT a sync return.
//
// Regression guard for the bug where `gen_nitro` emitted callback-interface
// methods with `return_kind.ts_type()` (no Promise wrapping) while regular
// interface methods used `ts_return_signature()` (which wraps async in
// `Promise<T>`). An `async fn` foreign-trait method must surface as
// `Promise<T>` so a JS implementation can be `async` and the Nitro runtime
// can await it before handing the value back to Rust; a sync return type is
// not assignable from an `async` JS method and breaks consumer typechecks.
//
// The test depends on `libasync_callbacks.so` being built somewhere under
// `target/`. If it's not, the test is skipped (rather than failing) so
// `cargo test --workspace` doesn't require building every fixture first.

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
fn nitro_async_callback_methods_return_promise() {
    let Some(lib) = find_built_cdylib("async_callbacks") else {
        eprintln!(
            "skipping: libasync_callbacks.so not found — \
             build `cargo build -p uniffi-fixture-async-callbacks` first"
        );
        return;
    };

    let out = tempfile::Builder::new()
        .prefix("ubrn-nitro-async-callbacks-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("tempdir")
        .keep();
    let ts_dir = Utf8PathBuf::from_path_buf(out.join("ts")).unwrap();
    let cpp_dir = Utf8PathBuf::from_path_buf(out.join("cpp")).unwrap();
    std::fs::create_dir_all(&ts_dir).unwrap();
    std::fs::create_dir_all(&cpp_dir).unwrap();
    eprintln!("nitro async-callbacks emit → {}", out.display());

    let switches = SwitchArgs {
        flavor: AbiFlavor::Nitro,
    };
    let source = SourceArgs::library(&lib);
    let output = OutputArgs::new(&ts_dir, &cpp_dir, /* no_format */ true);
    let args = BindingsArgs::new(switches, source, output);

    args.run(None).expect("nitro emission");

    let spec_path = ts_dir.join("AsyncCallbacks.nitro.ts");
    assert!(spec_path.exists(), "spec at {spec_path}");
    let spec = std::fs::read_to_string(&spec_path).unwrap();

    // The `with_foreign` trait surfaces as a HybridObject interface.
    assert!(
        spec.contains("export interface AsyncParser extends HybridObject"),
        "spec should declare AsyncParser HybridObject:\n{spec}"
    );

    // `async fn as_string(...) -> String` → `Promise<string>`.
    assert!(
        spec.contains("asString(delayMs: number, value: number): Promise<string>"),
        "async callback method as_string should return Promise<string>, got:\n{spec}"
    );

    // `async fn try_from_string(...) -> Result<i32, _>` → `Promise<number>`.
    assert!(
        spec.contains("): Promise<number>"),
        "async fallible callback method should return Promise<number>, got:\n{spec}"
    );

    // `async fn delay(...)` (void) → `Promise<void>`.
    assert!(
        spec.contains("delay(delayMs: number): Promise<void>"),
        "async void callback method delay should return Promise<void>, got:\n{spec}"
    );

    // Guard the actual bug: no callback method may surface its value type
    // bare (sync). The four methods are async, so none of these sync
    // spellings may appear in the AsyncParser block.
    assert!(
        !spec.contains("asString(delayMs: number, value: number): string"),
        "async callback method must not be emitted sync:\n{spec}"
    );

    // ---- C++ side: the callback vtable + trampolines must use the
    // foreign-future ABI, not the sync out-return ABI. The TS-only fix
    // (Promise<T>) typechecks but leaves the vtable struct layout wrong —
    // an ABI mismatch that is UB when Rust calls through the fn-pointer.
    let cpp_path = cpp_dir.join("HybridAsyncParser.cpp");
    assert!(cpp_path.exists(), "cpp at {cpp_path}");
    let cpp = std::fs::read_to_string(&cpp_path).unwrap();

    // The vtable struct's async-method fields must carry the foreign-future
    // signature: `ForeignFutureCallback<RetFfiType>` + a `uint64_t` data
    // handle + a `ForeignFutureDroppedCallbackStruct*` out-param, returning
    // void — matching uniffi's `<Trait>VTable` async field shape.
    assert!(
        cpp.contains("ForeignFutureCallback"),
        "callback vtable/trampolines must use the foreign-future callback ABI:\n{cpp}"
    );
    assert!(
        cpp.contains("ForeignFutureDroppedCallbackStruct"),
        "async vtable fields must take a ForeignFutureDroppedCallbackStruct* out-param:\n{cpp}"
    );

    // `as_string -> String` (RustBuffer FFI return): the async vtable field
    // must be `ForeignFutureCallback<RustBuffer>`, NOT a sync
    // `RustBuffer* uniffi_out_return` field.
    assert!(
        cpp.contains("void (*asString)(uint64_t, int32_t, int32_t,")
            && cpp.contains("::ubrn::nitro::ForeignFutureCallback<RustBuffer>,"),
        "asString vtable field must be the async foreign-future signature:\n{cpp}"
    );

    // The async trampoline must drive the JS Promise: register resolve +
    // reject continuations and fire `uniffi_callback(uniffi_callback_data, …)`.
    assert!(
        cpp.contains("addOnResolvedListener") && cpp.contains("addOnRejectedListener"),
        "async trampoline must register Promise resolve/reject continuations:\n{cpp}"
    );
    assert!(
        cpp.contains("uniffi_callback(uniffi_callback_data, result)"),
        "async trampoline must invoke the foreign-future callback with the result:\n{cpp}"
    );

    // `delay -> ()` (void FFI return): the async vtable field must be
    // `ForeignFutureCallback<void>`.
    assert!(
        cpp.contains("::ubrn::nitro::ForeignFutureCallback<void>,"),
        "void async callback method must use ForeignFutureCallback<void>:\n{cpp}"
    );

    // Regression guard: the async trampolines must NOT carry the sync
    // out-return shape. The sync trampoline declares a
    // `UniffiRustCallStatus* uniffi_out_call_status` param, writes
    // `*uniffi_out_return = …`, and sets `uniffi_out_call_status->code` —
    // none of those code constructs may appear for the (all-async)
    // AsyncParser methods. (The leading doc comment mentions the words to
    // contrast the two ABIs; match the load-bearing code spellings only.)
    assert!(
        !cpp.contains("UniffiRustCallStatus* uniffi_out_call_status"),
        "async trampolines must not declare a sync RustCallStatus out-param:\n{cpp}"
    );
    assert!(
        !cpp.contains("*uniffi_out_return =") && !cpp.contains("uniffi_out_call_status->"),
        "async trampolines must not write the sync out-return / status fields:\n{cpp}"
    );

    std::fs::remove_dir_all(out).ok();
}
