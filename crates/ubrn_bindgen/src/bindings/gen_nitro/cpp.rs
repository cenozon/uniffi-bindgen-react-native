/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
//! C++ emission for the Nitro backend. Three sets of files per namespace:
//!
//! 1. `Hybrid<Namespace>Api.{hpp,cpp}` — the namespace-level HybridObject
//!    that hosts all top-level functions as typed methods.
//!
//! 2. `Hybrid<Interface>.{hpp,cpp}` — one pair per uniffi interface,
//!    owning a `UniffiObjectHandle` over the Rust-side Arc-counted
//!    pointer. Methods invoke the uniffi C ABI directly.
//!
//! 3. `<namespace>_codecs.hpp` — the (currently empty for V1) header
//!    placeholder for record / enum codecs.

use std::collections::BTreeMap;

use anyhow::Result;
use askama::Template;
use camino::Utf8Path;

use super::model::{NitroCallbackInterface, NitroEnum, NitroInterface, NitroModule, NitroRecord};
use super::HybridObjectEntry;

/// Emit the per-record `<Name>.hpp` (struct definition + `JSIConverter`).
/// This is the source of truth ubrn owns directly — no Nitrogen.
pub(super) fn write_record(
    cpp_dir: &Utf8Path,
    module: &NitroModule,
    record: &NitroRecord,
) -> Result<()> {
    let text = RecordHpp { module, record }.render()?;
    let path = cpp_dir.join(format!("{}.hpp", record.ts_name));
    ubrn_common::write_file(path, text)?;
    // A record in a header `#include` cycle defines its `JSIConverter`
    // out-of-line in a guarded `<Name>.conv.hpp` footer (see `record.hpp` /
    // `NitroModule::resolve_cycles`). Acyclic records keep everything inline.
    if record.in_cycle() {
        let conv = RecordConvHpp { module, record }.render()?;
        let conv_path = cpp_dir.join(format!("{}.conv.hpp", record.ts_name));
        ubrn_common::write_file(conv_path, conv)?;
    }
    Ok(())
}

/// Emit the per-enum `<Name>.hpp` (flat `enum class` or tagged
/// `std::variant` representation + `JSIConverter`).
pub(super) fn write_enum(cpp_dir: &Utf8Path, module: &NitroModule, en: &NitroEnum) -> Result<()> {
    let text = EnumHpp { module, en }.render()?;
    let path = cpp_dir.join(format!("{}.hpp", en.ts_name));
    ubrn_common::write_file(path, text)?;
    // A tagged enum in a header `#include` cycle defines its `JSIConverter`
    // out-of-line in a guarded `<Name>.conv.hpp` footer (see `enum.hpp`).
    // Acyclic / flat enums keep everything inline.
    if en.in_cycle() {
        let conv = EnumConvHpp { module, en }.render()?;
        let conv_path = cpp_dir.join(format!("{}.conv.hpp", en.ts_name));
        ubrn_common::write_file(conv_path, conv)?;
    }
    Ok(())
}

pub(super) fn write_namespace_api(cpp_dir: &Utf8Path, module: &NitroModule) -> Result<()> {
    let hpp_text = NamespaceApiHpp { module }.render()?;
    let cpp_text = NamespaceApiCpp { module }.render()?;
    let hpp_path = cpp_dir.join(format!("{}.hpp", module.namespace_api_cxx_class()));
    let cpp_path = cpp_dir.join(format!("{}.cpp", module.namespace_api_cxx_class()));
    ubrn_common::write_file(hpp_path, hpp_text)?;
    ubrn_common::write_file(cpp_path, cpp_text)?;
    Ok(())
}

pub(super) fn write_interface(
    cpp_dir: &Utf8Path,
    module: &NitroModule,
    iface: &NitroInterface,
) -> Result<()> {
    let hpp_text = InterfaceHpp { module, iface }.render()?;
    let cpp_text = InterfaceCpp { module, iface }.render()?;
    let hpp_path = cpp_dir.join(format!("{}.hpp", iface.cxx_class));
    let cpp_path = cpp_dir.join(format!("{}.cpp", iface.cxx_class));
    ubrn_common::write_file(hpp_path, hpp_text)?;
    ubrn_common::write_file(cpp_path, cpp_text)?;
    Ok(())
}

pub(super) fn write_codecs(cpp_dir: &Utf8Path, module: &NitroModule) -> Result<()> {
    let text = CodecsHpp { module }.render()?;
    let path = cpp_dir.join(module.codecs_header_filename());
    ubrn_common::write_file(path, text)?;
    Ok(())
}

/// Emit the project-level `register_natives.cpp` glue file. Walks the
/// collected [`HybridObjectEntry`] list and registers every constructor
/// with Nitro's process-global `HybridObjectRegistry`. Registration runs
/// automatically at library-load time via a static initializer (mobile +
/// desktop), and an idempotent `extern "C" void registerNatives(jsi::Runtime&)`
/// is also exposed for the desktop test runner. A `std::once_flag` ensures
/// registration happens exactly once across both entry points. This replaces
/// nitrogen's `OnLoad`/autolinking — nitrogen is never invoked.
///
/// Always emits a file — an empty `hybrid_objects` slice produces a
/// well-formed but no-op function so downstream `add_library` source
/// lists never have a dangling reference.
pub(super) fn write_register_natives(
    cpp_dir: &Utf8Path,
    entries: &[HybridObjectEntry],
) -> Result<()> {
    let text = RegisterNativesCpp {
        hybrid_objects: entries,
    }
    .render()?;
    let path = cpp_dir.join("register_natives.cpp");
    ubrn_common::write_file(path, text)?;
    Ok(())
}

/// Emit the iOS-only unity/amalgamation chunks. Each chunk `#include`s a
/// disjoint subset of the per-object `Hybrid*.cpp` (which stay in
/// `cpp_dir`); iOS compiles only these chunks (see nitro-podspec.rb),
/// collapsing ~N heavy TUs to ~K so per-TU shared-header DWARF stops
/// overflowing libtool's 32-bit Mach-O archive. Android is unaffected
/// (its CMakeLists enumerates the per-object `.cpp` directly and never
/// references `amalgam/`). `register_natives.cpp` is intentionally NOT in
/// `entries`, so it stays its own TU and its load-time static initializer
/// is defined exactly once. Chunks are grouped per `cxx_namespace` and
/// partitioned round-robin over the already-name-sorted `entries` so the
/// largest object does not cluster.
pub(super) fn write_amalgam_chunks(
    cpp_dir: &Utf8Path,
    entries: &[HybridObjectEntry],
) -> Result<()> {
    let k = super::K_AMALGAM_CHUNKS;
    // Chunks live in a dedicated subdir; `write_file` does not create parents.
    let amalgam_dir = cpp_dir.join("amalgam");
    ubrn_common::mk_dir(&amalgam_dir)?;
    // Group by namespace, preserving the incoming (sorted) order.
    let mut by_ns: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in entries {
        by_ns
            .entry(e.cxx_namespace.as_str())
            .or_default()
            .push(e.cxx_class.as_str());
    }
    for (ns, classes) in &by_ns {
        // Round-robin partition into k buckets; skip empties.
        let mut buckets: Vec<Vec<String>> = vec![Vec::new(); k];
        for (i, cxx_class) in classes.iter().enumerate() {
            buckets[i % k].push((*cxx_class).to_string());
        }
        for (i, bucket) in buckets.iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            let text = AmalgamChunkCpp { includes: bucket }.render()?;
            let path = amalgam_dir.join(format!("{ns}_chunk_{i:02}.cpp"));
            ubrn_common::write_file(path, text)?;
        }
    }
    Ok(())
}

/// Emit the per-callback-interface trampoline pair
/// (`Hybrid<Name>.{hpp,cpp}`) for one callback interface. The hpp
/// surfaces the `ensure_<Name>_vtable_init` hook; the cpp defines the
/// per-method trampolines that Rust dispatches through.
pub(super) fn write_callback_interface(
    cpp_dir: &Utf8Path,
    module: &NitroModule,
    cb: &NitroCallbackInterface,
) -> Result<()> {
    let hpp_text = CallbackHpp { module, cb }.render()?;
    let cpp_text = CallbackCpp { module, cb }.render()?;
    let hpp_path = cpp_dir.join(format!("{}.hpp", cb.cxx_class));
    let cpp_path = cpp_dir.join(format!("{}.cpp", cb.cxx_class));
    ubrn_common::write_file(hpp_path, hpp_text)?;
    ubrn_common::write_file(cpp_path, cpp_text)?;
    Ok(())
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "namespace_api.hpp")]
struct NamespaceApiHpp<'a> {
    module: &'a NitroModule,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "namespace_api.cpp")]
struct NamespaceApiCpp<'a> {
    module: &'a NitroModule,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "interface.hpp")]
struct InterfaceHpp<'a> {
    module: &'a NitroModule,
    iface: &'a NitroInterface,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "interface.cpp")]
struct InterfaceCpp<'a> {
    module: &'a NitroModule,
    iface: &'a NitroInterface,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "codecs.hpp")]
struct CodecsHpp<'a> {
    module: &'a NitroModule,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "record.hpp")]
struct RecordHpp<'a> {
    module: &'a NitroModule,
    record: &'a NitroRecord,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "record_conv.hpp")]
struct RecordConvHpp<'a> {
    module: &'a NitroModule,
    record: &'a NitroRecord,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "enum.hpp")]
struct EnumHpp<'a> {
    module: &'a NitroModule,
    en: &'a NitroEnum,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "enum_conv.hpp")]
struct EnumConvHpp<'a> {
    module: &'a NitroModule,
    en: &'a NitroEnum,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "callback.hpp")]
struct CallbackHpp<'a> {
    module: &'a NitroModule,
    cb: &'a NitroCallbackInterface,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "callback.cpp")]
struct CallbackCpp<'a> {
    module: &'a NitroModule,
    cb: &'a NitroCallbackInterface,
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "register_natives.cpp")]
struct RegisterNativesCpp<'a> {
    hybrid_objects: &'a [HybridObjectEntry],
}

#[derive(Template)]
#[template(syntax = "cpp", escape = "none", path = "amalgam_chunk.cpp")]
struct AmalgamChunkCpp<'a> {
    includes: &'a [String],
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::gen_nitro::HybridObjectKind;
    use camino::Utf8PathBuf;

    fn entry(name: &str, ns: &str) -> HybridObjectEntry {
        HybridObjectEntry {
            name: name.to_string(),
            cxx_class: format!("Hybrid{name}"),
            cxx_namespace: ns.to_string(),
            kind: HybridObjectKind::Interface,
        }
    }

    #[test]
    fn write_amalgam_chunks_partitions_deterministically() {
        // Many objects in one namespace: exercises the round-robin split.
        let entries: Vec<HybridObjectEntry> =
            (0..20).map(|i| entry(&format!("Obj{i:02}"), "acme")).collect();

        let tmp = tempfile::tempdir().unwrap();
        let cpp_dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_amalgam_chunks(&cpp_dir, &entries).unwrap();

        let amalgam = cpp_dir.join("amalgam");
        let mut chunk_files: Vec<String> = std::fs::read_dir(&amalgam)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        chunk_files.sort();

        // At most K chunks, and they follow the `<ns>_chunk_NN.cpp` shape.
        assert!(
            chunk_files.len() <= super::super::K_AMALGAM_CHUNKS,
            "emitted {} chunks, expected <= {}",
            chunk_files.len(),
            super::super::K_AMALGAM_CHUNKS
        );
        for f in &chunk_files {
            assert!(
                f.starts_with("acme_chunk_") && f.ends_with(".cpp"),
                "unexpected chunk filename: {f}"
            );
            // NN is exactly two digits.
            let nn = &f["acme_chunk_".len()..f.len() - ".cpp".len()];
            assert_eq!(nn.len(), 2, "chunk index not zero-padded 2 digits: {f}");
            assert!(nn.chars().all(|c| c.is_ascii_digit()), "non-numeric NN: {f}");
        }

        // Every per-object cxx_class appears in exactly one chunk via a
        // `#include "../<cxx_class>.cpp"` line; register_natives.cpp never does.
        let mut seen: std::collections::BTreeMap<String, usize> = Default::default();
        for f in &chunk_files {
            let body = std::fs::read_to_string(amalgam.join(f)).unwrap();
            assert!(
                !body.contains("register_natives.cpp"),
                "register_natives.cpp must not be amalgamated: {f}"
            );
            for entry in &entries {
                let needle = format!("#include \"../{}.cpp\"", entry.cxx_class);
                if body.contains(&needle) {
                    *seen.entry(entry.cxx_class.clone()).or_default() += 1;
                }
            }
        }
        for entry in &entries {
            assert_eq!(
                seen.get(&entry.cxx_class).copied().unwrap_or(0),
                1,
                "{} must appear in exactly one chunk",
                entry.cxx_class
            );
        }
    }

    #[test]
    fn write_amalgam_chunks_groups_per_namespace_and_skips_empty() {
        // Two namespaces, few objects each -> chunk filenames carry the ns and
        // empty buckets are skipped (fewer than K files per ns).
        let entries = vec![
            entry("Alpha", "one"),
            entry("Beta", "one"),
            entry("Gamma", "two"),
        ];
        let tmp = tempfile::tempdir().unwrap();
        let cpp_dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_amalgam_chunks(&cpp_dir, &entries).unwrap();

        let amalgam = cpp_dir.join("amalgam");
        let files: Vec<String> = std::fs::read_dir(&amalgam)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        // 2 ns-`one` objects -> 2 chunks; 1 ns-`two` object -> 1 chunk; empty
        // buckets skipped.
        assert_eq!(files.len(), 3, "got {files:?}");
        assert_eq!(files.iter().filter(|f| f.starts_with("one_chunk_")).count(), 2);
        assert_eq!(files.iter().filter(|f| f.starts_with("two_chunk_")).count(), 1);
    }
}
