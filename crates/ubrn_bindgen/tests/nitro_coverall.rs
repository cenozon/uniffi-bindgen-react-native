// SPDX-License-Identifier: MPL-2.0
//
// End-to-end test for the Nitro backend's record + enum emission. Drives
// `BindingsArgs::run` with `AbiFlavor::Nitro` against the `coverall`
// fixture's freshly-built cdylib and verifies the emitted TS spec
// contains `export interface` declarations for each uniffi record and
// `export type` string-unions for each flat enum, plus matching
// `lift_<Name>` / `lower_<Name>` free functions in the C++ codecs header.
//
// Mirrors `nitro_arithmetic.rs`'s skip-if-missing-cdylib pattern so
// `cargo test --workspace` doesn't require building every fixture
// first. To run end-to-end:
//
//     cargo build -p uniffi-fixture-coverall
//     cargo test -p ubrn_bindgen --test nitro_coverall
//
// The coverall fixture exposes (among others):
//
//   * dictionary `SimpleDict`  — a record with mixed primitive +
//     unsupported-by-V2 field types (Optional / Bytes / Interface). The
//     emitter falls back to a Stub placeholder for those fields and
//     flags the record's codec as "not yet ready", but the TS spec
//     still gets the full `export interface` shape. The C++ codecs
//     header still emits `lift_SimpleDict` / `lower_SimpleDict`
//     prototypes — they just throw at runtime.
//   * dictionary `EmptyStruct` — a zero-field record. Codec round-trips
//     cleanly.
//   * dictionary `Repair`      — has a single nested-interface field
//     (`Patch`), so its codec is not yet ready either; TS interface
//     emits fine.
//   * enum `Color`             — a clean flat enum (Red/Blue/Green).
//     Both the TS string-union and the i32-ordinal codec emit cleanly.

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
fn nitro_emit_against_coverall_cdylib() {
    let Some(lib) = find_built_cdylib("uniffi_coverall") else {
        eprintln!(
            "skipping: libuniffi_coverall.so not found — \
             build `cargo build -p uniffi-fixture-coverall` first"
        );
        return;
    };

    let out = tempfile::Builder::new()
        .prefix("ubrn-nitro-coverall-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("tempdir")
        .keep();
    let ts_dir = Utf8PathBuf::from_path_buf(out.join("ts")).unwrap();
    let cpp_dir = Utf8PathBuf::from_path_buf(out.join("cpp")).unwrap();
    std::fs::create_dir_all(&ts_dir).unwrap();
    std::fs::create_dir_all(&cpp_dir).unwrap();
    eprintln!("nitro emit (coverall) -> {}", out.display());

    let switches = SwitchArgs {
        flavor: AbiFlavor::Nitro,
    };
    let source = SourceArgs::library(&lib);
    let output = OutputArgs::new(&ts_dir, &cpp_dir, /* no_format */ true);
    let args = BindingsArgs::new(switches, source, output);

    let outcome = args.run(None).expect("nitro emission");
    assert!(
        !outcome.modules.is_empty(),
        "coverall must produce at least one module"
    );

    // ---- TS spec: records & enums ----
    let spec_path = ts_dir.join("Coverall.nitro.ts");
    assert!(spec_path.exists(), "missing TS spec at {spec_path}");
    let spec = std::fs::read_to_string(&spec_path).unwrap();

    // Records become `export type <Name> = {` blocks (audit bug #13) — the
    // canonical JSI surface, not the old `export interface`. SimpleDict and
    // EmptyStruct are the canary cases.
    assert!(
        spec.contains("export type SimpleDict = {"),
        "spec should declare SimpleDict as a `export type` record; got:\n{spec}"
    );
    assert!(
        spec.contains("export type EmptyStruct = {"),
        "spec should declare EmptyStruct as a `export type` record; got:\n{spec}"
    );

    // Flat enums become string-valued runtime `export enum` values (audit bug
    // #11), NOT type-only unions — so `Color.Red` resolves at runtime. Color is
    // the canary case. The exact whitespace varies, so match the substring
    // shape rather than the full decl text.
    assert!(
        spec.contains("export enum Color {"),
        "spec should declare Color as a runtime `export enum`; got:\n{spec}"
    );
    assert!(
        spec.contains("Red = 'Red'")
            && spec.contains("Blue = 'Blue'")
            && spec.contains("Green = 'Green'"),
        "Color enum should give each variant a string value; got:\n{spec}"
    );

    // ---- C++ codecs header: lift/lower free functions ----
    let codecs_path = cpp_dir.join("coverall_codecs.hpp");
    assert!(codecs_path.exists(), "missing codecs at {codecs_path}");
    let codecs = std::fs::read_to_string(&codecs_path).unwrap();

    // Records get prototypes regardless of `codec_ready`.
    for record in ["SimpleDict", "EmptyStruct"] {
        let lift_decl = format!("lift_{record}");
        let lower_decl = format!("lower_{record}");
        assert!(
            codecs.contains(&lift_decl),
            "codecs should declare {lift_decl}; got:\n{codecs}"
        );
        assert!(
            codecs.contains(&lower_decl),
            "codecs should declare {lower_decl}; got:\n{codecs}"
        );
    }

    // Flat enums get matching lift/lower prototypes too.
    assert!(
        codecs.contains("lift_Color"),
        "codecs should declare lift_Color; got:\n{codecs}"
    );
    assert!(
        codecs.contains("lower_Color"),
        "codecs should declare lower_Color; got:\n{codecs}"
    );

    // The namespace API impl file pulls the codecs header in so each
    // method body's `lower_*` / `lift_*` expressions resolve.
    let api_cpp_path = cpp_dir.join("HybridCoverallApi.cpp");
    if api_cpp_path.exists() {
        let api_cpp = std::fs::read_to_string(&api_cpp_path).unwrap();
        assert!(
            api_cpp.contains("coverall_codecs.hpp"),
            "namespace API impl should include the codecs header; got:\n{api_cpp}"
        );
    }

    // Success — clean up the persistent emit dir.
    std::fs::remove_dir_all(out).ok();
}
