// SPDX-License-Identifier: MPL-2.0
//
// End-to-end test: drive `BindingsArgs::run` with `AbiFlavor::Nitro`
// against the `callbacks` fixture crate's freshly-built cdylib and
// verify that:
//
//   * Composite types (Option / Sequence / Map / Bytes) emit the
//     expected `lift_*` / `lower_*` expressions and TS type spellings.
//   * Each `callback interface` becomes a TS HybridObject interface
//     plus a `Hybrid<Name>.{hpp,cpp}` trampoline pair on the C++ side.
//   * Method parameters typed as a callback interface use
//     `std::shared_ptr<Hybrid<Name>Spec>` on the C++ side and
//     `<Name>` on the TS side, and the lowering expression registers
//     the instance with the `CallbackHandleMap`.
//
// The test depends on `libuniffi_fixture_callbacks.so` being built
// somewhere under `target/`. If it's not, the test is skipped (rather
// than failing) so `cargo test --workspace` doesn't require building
// every fixture first.

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
fn nitro_emit_against_callbacks_cdylib() {
    let Some(lib) = find_built_cdylib("uniffi_fixture_callbacks") else {
        eprintln!(
            "skipping: libuniffi_fixture_callbacks.so not found — \
             build `cargo build -p uniffi-fixture-callbacks` first"
        );
        return;
    };

    let out = tempfile::Builder::new()
        .prefix("ubrn-nitro-callbacks-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("tempdir")
        .keep();
    let ts_dir = Utf8PathBuf::from_path_buf(out.join("ts")).unwrap();
    let cpp_dir = Utf8PathBuf::from_path_buf(out.join("cpp")).unwrap();
    std::fs::create_dir_all(&ts_dir).unwrap();
    std::fs::create_dir_all(&cpp_dir).unwrap();
    eprintln!("nitro callbacks emit → {}", out.display());

    let switches = SwitchArgs {
        flavor: AbiFlavor::Nitro,
    };
    let source = SourceArgs::library(&lib);
    let output = OutputArgs::new(&ts_dir, &cpp_dir, /* no_format */ true);
    let args = BindingsArgs::new(switches, source, output);

    let outcome = args.run(None).expect("nitro emission");
    assert_eq!(
        outcome.modules.len(),
        1,
        "callbacks fixture has a single namespace"
    );

    // ---- Autolinking: callback interfaces show up as HybridObject entries ----
    let names: Vec<&str> = outcome
        .nitro_hybrid_objects
        .iter()
        .map(|h| h.name.as_str())
        .collect();
    assert!(
        names.contains(&"ForeignGetters"),
        "expected ForeignGetters callback HybridObject in {names:?}"
    );
    assert!(
        names.contains(&"StoredForeignStringifier"),
        "expected StoredForeignStringifier callback HybridObject in {names:?}"
    );

    // ---- .nitro.ts spec ----
    let spec_path = ts_dir.join("Callbacks.nitro.ts");
    assert!(spec_path.exists(), "spec at {spec_path}");
    let spec = std::fs::read_to_string(&spec_path).unwrap();

    // Callback interfaces declared as HybridObjects with their methods.
    assert!(
        spec.contains("export interface ForeignGetters extends HybridObject"),
        "spec should declare ForeignGetters HybridObject:\n{spec}"
    );
    assert!(
        spec.contains("export interface StoredForeignStringifier extends HybridObject"),
        "spec should declare StoredForeignStringifier HybridObject:\n{spec}"
    );

    // Composite types come through with the expected TS spellings.
    //   `string?` → `(string) | undefined`
    //   `sequence<i32>` → `(number)[]`
    //   `sequence<f64?>?` → `((number) | undefined)[] | undefined`
    //   Optionals spell `| undefined` (not `| null`) because Nitro's
    //   `JSIConverter<std::optional<T>>` maps `nullopt` to/from JS `undefined`
    //   only; the exact `(x) | undefined` parenthesization matters for the
    //   recursive composer.
    assert!(
        spec.contains("| undefined"),
        "spec should mention optional type:\n{spec}"
    );
    assert!(
        spec.contains(")[]"),
        "spec should mention array type:\n{spec}"
    );
    // `Uint8Array` only appears if a bytes type is present in callbacks
    // — this fixture doesn't use one, so we don't assert that.

    // Method parameters typed as a callback interface use the bare name.
    assert!(
        spec.contains("callback: ForeignGetters"),
        "spec should accept ForeignGetters as a method arg:\n{spec}"
    );

    // ---- Consumer-facing TS module re-exports callback types ----
    let reexport_path = ts_dir.join("callbacks.ts");
    assert!(reexport_path.exists());
    let reexport = std::fs::read_to_string(&reexport_path).unwrap();
    assert!(
        reexport.contains("ForeignGetters"),
        "reexport should re-export ForeignGetters type:\n{reexport}"
    );

    // ---- C++ namespace API impl uses shared_ptr<Hybrid*Spec> for callback args ----
    let interface_cpp = cpp_dir.join("HybridRustGetters.cpp");
    assert!(
        interface_cpp.exists(),
        "expected HybridRustGetters.cpp at {interface_cpp}"
    );
    let cpp = std::fs::read_to_string(&interface_cpp).unwrap();
    // The callback's `Hybrid<Name>` C++ type is namespace-qualified (every
    // generated class lives in `margelo::nitro::<ns>`), mirroring how a plain
    // interface arg is spelled — an absolute path resolves from any emitting
    // file, same-namespace or cross-crate.
    assert!(
        cpp.contains("std::shared_ptr<::margelo::nitro::callbacks::HybridForeignGetters>"),
        "RustGetters methods should take a qualified shared_ptr<HybridForeignGetters>:\n{cpp}"
    );
    // Lowering for a callback arg installs the vtable then turns the
    // shared_ptr into the u64 handle via the class's `lower_to_handle`
    // surface (which registers a JS impl with the CallbackHandleMap / clones
    // a proxy handle).
    assert!(
        cpp.contains("::margelo::nitro::callbacks::HybridForeignGetters::ensure_vtable()")
            && cpp.contains("->lower_to_handle("),
        "callback arg lowering should install the vtable + lower to a handle:\n{cpp}"
    );
    // Optional / sequence lowering for the composite-typed RustGetters
    // methods. The fixture's `get_option(string? v, ...)` and
    // `get_list(sequence<i32> v, ...)` both flow through here.
    assert!(
        cpp.contains("lower_optional<"),
        "expected lower_optional invocation in {interface_cpp}:\n{cpp}"
    );
    assert!(
        cpp.contains("lower_sequence<"),
        "expected lower_sequence invocation in {interface_cpp}:\n{cpp}"
    );

    // ---- Callback trampoline header + cpp got emitted ----
    let cb_hpp = cpp_dir.join("HybridForeignGetters.hpp");
    let cb_cpp = cpp_dir.join("HybridForeignGetters.cpp");
    assert!(
        cb_hpp.exists(),
        "expected callback trampoline header at {cb_hpp}"
    );
    assert!(
        cb_cpp.exists(),
        "expected callback trampoline impl at {cb_cpp}"
    );
    let cb_cpp_text = std::fs::read_to_string(&cb_cpp).unwrap();

    // The vtable init hook is declared and idempotently invoked.
    assert!(
        cb_cpp_text.contains("ensure_ForeignGetters_vtable_init"),
        "expected vtable-init hook in {cb_cpp}:\n{cb_cpp_text}"
    );
    // A per-method trampoline exists for each callback method.
    for method in ["getBool", "getString", "getOption", "getList", "getNothing"] {
        let needle = format!("ForeignGetters_trampoline_{method}");
        assert!(
            cb_cpp_text.contains(&needle),
            "expected {needle} trampoline in {cb_cpp}:\n{cb_cpp_text}"
        );
    }
    // Clone + free trampolines for the lifetime contract with Rust.
    assert!(
        cb_cpp_text.contains("ForeignGetters_trampoline_clone"),
        "expected clone trampoline in {cb_cpp}:\n{cb_cpp_text}"
    );
    assert!(
        cb_cpp_text.contains("ForeignGetters_trampoline_free"),
        "expected free trampoline in {cb_cpp}:\n{cb_cpp_text}"
    );
    // The Rust-side vtable init symbol is referenced via extern "C".
    assert!(
        cb_cpp_text.contains("uniffi_") && cb_cpp_text.contains("_vtable_init"),
        "expected uniffi vtable-init symbol referenced in {cb_cpp}:\n{cb_cpp_text}"
    );

    // ---- StoredForeignStringifier's get_complex `sequence<f64?>?` exercises
    //      nested composites — verify its trampoline lifts an optional
    //      sequence of optionals correctly.
    let stringifier_cpp = cpp_dir.join("HybridStoredForeignStringifier.cpp");
    assert!(stringifier_cpp.exists());
    let stringifier_text = std::fs::read_to_string(&stringifier_cpp).unwrap();
    assert!(
        stringifier_text.contains("lift_optional<")
            && stringifier_text.contains("read_sequence<")
            && stringifier_text.contains("read_optional<"),
        "expected nested-composite lift expressions in {stringifier_cpp}:\n{stringifier_text}"
    );

    std::fs::remove_dir_all(out).ok();
}
