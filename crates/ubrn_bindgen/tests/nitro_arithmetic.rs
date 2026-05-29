// SPDX-License-Identifier: MPL-2.0
//
// End-to-end test: drive `BindingsArgs::run` with `AbiFlavor::Nitro`
// against the `arithmetical` example crate's freshly-built cdylib and
// verify the emitted files exist + look syntactically sane.
//
// The test depends on `libarithmetical.so` being built somewhere under
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
fn nitro_emit_against_arithmetic_cdylib() {
    let Some(lib) = find_built_cdylib("arithmetical") else {
        eprintln!("skipping: libarithmetical.so not found — build `cargo build -p uniffi-example-arithmetic` first");
        return;
    };

    // Use cargo's per-test-binary temp dir — set on every platform Cargo
    // supports, so this works on Windows without hand-rolling a path.
    // `tempfile::Builder::keep` leaves the dir on disk after the test so
    // failures are inspectable; success path cleans up explicitly below.
    let out = tempfile::Builder::new()
        .prefix("ubrn-nitro-emit-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("tempdir")
        .keep();
    let ts_dir = Utf8PathBuf::from_path_buf(out.join("ts")).unwrap();
    let cpp_dir = Utf8PathBuf::from_path_buf(out.join("cpp")).unwrap();
    std::fs::create_dir_all(&ts_dir).unwrap();
    std::fs::create_dir_all(&cpp_dir).unwrap();
    eprintln!("nitro emit → {}", out.display());

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
        "arithmetic has a single namespace"
    );
    // The Nitro emission must surface its HybridObject list so the
    // project-level templates can populate `nitro.json#autolinking` and
    // the Android `CMakeLists.txt` source list. The `ArithmeticApi`
    // namespace HybridObject is always present; the test also exercises
    // the `Sub` interface, which contributes `Hybrid` impl + entry.
    assert!(
        outcome
            .nitro_hybrid_objects
            .iter()
            .any(|h| h.name == "ArithmeticApi" && h.cxx_class == "HybridArithmeticApi"),
        "expected ArithmeticApi HybridObject entry, got {:?}",
        outcome.nitro_hybrid_objects
    );

    // .nitro.ts spec
    let spec_path = ts_dir.join("Arithmetic.nitro.ts");
    assert!(spec_path.exists(), "spec at {spec_path}");
    let spec = std::fs::read_to_string(&spec_path).unwrap();
    assert!(spec.contains("export interface ArithmeticApi extends HybridObject"));
    assert!(spec.contains("add("));
    assert!(spec.contains("sub("));
    assert!(spec.contains("div("));
    assert!(spec.contains("equal("));
    assert!(spec.contains(": bigint"));
    assert!(spec.contains(": boolean"));

    // Consumer-facing TS
    let reexport_path = ts_dir.join("arithmetic.ts");
    assert!(reexport_path.exists());
    let reexport = std::fs::read_to_string(&reexport_path).unwrap();
    assert!(reexport.contains("export function arithmetic()"));
    assert!(reexport.contains("NitroModules.createHybridObject<ArithmeticApi>"));

    // C++ impl class
    let api_hpp = cpp_dir.join("HybridArithmeticApi.hpp");
    let api_cpp = cpp_dir.join("HybridArithmeticApi.cpp");
    assert!(api_hpp.exists());
    assert!(api_cpp.exists());
    let cpp = std::fs::read_to_string(&api_cpp).unwrap();
    assert!(cpp.contains("uint64_t HybridArithmeticApi::add"));
    assert!(cpp.contains("uniffi_arithmetical_fn_func_add"));
    assert!(cpp.contains("ubrn::nitro::make_status()"));
    assert!(cpp.contains("ubrn::nitro::check_status"));

    // `register_natives.cpp` is the host-runner entry point. It must
    // exist, expose the `extern "C" registerNatives` symbol, and call
    // `registerHybridObjectConstructor` for the namespace API HybridObject
    // (and any per-interface impls — `ArithmeticApi` is the
    // always-present one we can assert on without baking in fixture-
    // specific interface names).
    let register_natives_path = cpp_dir.join("register_natives.cpp");
    assert!(
        register_natives_path.exists(),
        "register_natives.cpp at {register_natives_path}"
    );
    let register_natives = std::fs::read_to_string(&register_natives_path).unwrap();
    assert!(
        register_natives.contains("extern \"C\" void registerNatives"),
        "register_natives.cpp missing `extern \"C\" void registerNatives` entry point:\n{register_natives}"
    );
    // Whitespace-insensitive match: the Askama template wraps the call
    // across multiple lines, so collapse all whitespace before the
    // substring check.
    let normalized: String = register_natives
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        normalized.contains("registerHybridObjectConstructor( \"ArithmeticApi\""),
        "register_natives.cpp missing `registerHybridObjectConstructor(\"ArithmeticApi\", ...)` call:\n{register_natives}"
    );
    assert!(
        register_natives.contains("HybridArithmeticApi"),
        "register_natives.cpp missing `HybridArithmeticApi` impl class reference:\n{register_natives}"
    );

    // Success — clean up the persistent emit dir.
    std::fs::remove_dir_all(out).ok();
}
