// SPDX-License-Identifier: MPL-2.0
//
// End-to-end test: drive `BindingsArgs::run` with `AbiFlavor::Nitro`
// against the multi-namespace `ext-types` fixture and verify that a
// `.nitro.ts` spec which references a record / enum defined in a *sibling*
// namespace emits an `import type { … } from './<OtherNamespace>.nitro'`
// for it.
//
// Regression guard for the bug where the TS spec template imported only
// `HybridObject` and never emitted cross-namespace type imports, so a
// reference to a foreign record (e.g. `UniffiOneType` from the `uniffi_one`
// namespace, used by the `uniffi_ext_types_lib` namespace) produced a
// `TS2304 Cannot find name` against itself. The TS spec has no `export *`
// re-export glue between sibling `.nitro.ts` files, so every referenced
// foreign type must be imported by its own module.
//
// The test depends on `libuniffi_ext_types_lib.so` being built somewhere
// under `target/`; it is skipped if absent.

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
fn nitro_cross_namespace_types_are_imported() {
    let Some(lib) = find_built_cdylib("uniffi_ext_types_lib") else {
        eprintln!(
            "skipping: libuniffi_ext_types_lib.so not found — \
             build `cargo build -p uniffi-fixture-ext-types` first"
        );
        return;
    };

    let out = tempfile::Builder::new()
        .prefix("ubrn-nitro-ext-types-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("tempdir")
        .keep();
    let ts_dir = Utf8PathBuf::from_path_buf(out.join("ts")).unwrap();
    let cpp_dir = Utf8PathBuf::from_path_buf(out.join("cpp")).unwrap();
    std::fs::create_dir_all(&ts_dir).unwrap();
    std::fs::create_dir_all(&cpp_dir).unwrap();
    eprintln!("nitro ext-types emit → {}", out.display());

    let switches = SwitchArgs {
        flavor: AbiFlavor::Nitro,
    };
    let source = SourceArgs::library(&lib);
    let output = OutputArgs::new(&ts_dir, &cpp_dir, /* no_format */ true);
    let args = BindingsArgs::new(switches, source, output);

    let outcome = args.run(None).expect("nitro emission");
    assert!(
        outcome.modules.len() > 1,
        "ext-types is a multi-namespace fixture"
    );

    // The main namespace's spec (`imported_types_lib`) references
    // `UniffiOneType` (a record defined in the `uniffi_one_ns` namespace),
    // so it must import it from that module's `.nitro` spec.
    let spec_path = ts_dir.join("ImportedTypesLib.nitro.ts");
    assert!(spec_path.exists(), "spec at {spec_path}");
    let spec = std::fs::read_to_string(&spec_path).unwrap();

    assert!(
        spec.contains("from './UniffiOneNs.nitro'"),
        "spec should import cross-namespace types from the uniffi_one_ns module:\n{spec}"
    );
    // The imported set must name the actually-referenced type.
    let import_line = spec
        .lines()
        .find(|l| l.contains("from './UniffiOneNs.nitro'"))
        .unwrap_or("");
    assert!(
        import_line.contains("import type {") && import_line.contains("UniffiOneType"),
        "cross-namespace import should be a type-only import naming UniffiOneType, got: {import_line}"
    );

    std::fs::remove_dir_all(out).ok();
}
