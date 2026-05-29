/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
//! Per-uniffi-interface HybridObject codegen for the Nitro backend.
//!
//! This module is the "real" Nitro path — it replaces the JSI-host-object
//! middle layer with one Nitro HybridObject per uniffi interface, plus a
//! single namespace-scoped HybridObject (`<Namespace>Api`) that exposes
//! the namespace's top-level functions and constructors as typed methods.
//!
//! ## Emitted artifacts (per uniffi namespace)
//!
//! * `<ts_dir>/<namespace>.nitro.ts` — Nitro spec file: declares every
//!   `HybridObject` interface (one per uniffi `Object`, plus the namespace
//!   API HybridObject), every TS struct interface (one per uniffi
//!   `Record`), and every TS enum (one per uniffi `Enum`). Nitrogen
//!   consumes this file to emit the platform-specific `HybridXxxSpec`
//!   base classes + per-record `JSIConverter` specializations.
//!
//! * `<ts_dir>/<namespace>.ts` — TS re-export module that imports types
//!   from the `.nitro.ts` file and instantiates the namespace API
//!   HybridObject via `NitroModules.createHybridObject(...)`, so consumer
//!   code sees a familiar `import { add } from '<namespace>'` surface.
//!
//! * `<cpp_dir>/Hybrid<Namespace>Api.{hpp,cpp}` — the C++ impl class for
//!   the namespace API HybridObject. Each method body invokes the
//!   corresponding uniffi C-ABI symbol via the nitro-uniffi
//!   helpers (`lower_*` / `lift_*` / `check_status`).
//!
//! * `<cpp_dir>/Hybrid<Interface>.{hpp,cpp}` — one pair per uniffi
//!   `Object`. The class owns a `UniffiObjectHandle` (RAII over the
//!   opaque Arc-counted Rust handle) and delegates each method to the
//!   uniffi C-ABI symbol.
//!
//! * `<cpp_dir>/<namespace>_codecs.hpp` — RustBuffer codecs for records
//!   and enums in this namespace.
//!
//! ## Emitted at project level (across all namespaces)
//!
//! The `nitro.json` autolinking manifest is emitted by the project-level
//! template layer (see `ubrn_cli/src/jsi/nitro/codegen.rs`) once it knows
//! the full set of HybridObjects emitted by this module — that list is
//! returned from [`generate_all`] alongside the per-module
//! [`ModuleMetadata`].

mod cpp;
mod model;
mod ts;

use std::collections::BTreeSet;

use anyhow::Result;
use camino::Utf8Path;

use uniffi_bindgen::pipeline::general;

use crate::bindings::metadata::ModuleMetadata;

pub use self::model::HybridObjectKind;
use self::model::NitroModule;

/// Result of a `generate_all` invocation. Carries the per-namespace
/// [`ModuleMetadata`] (for the build flow that needs to iterate them) plus
/// the deduplicated set of HybridObject TS names that ubrn-bindgen
/// emitted; the project-level template layer uses that set to populate
/// the `autolinking` block of `nitro.json`.
pub struct NitroEmission {
    pub modules: Vec<ModuleMetadata>,
    pub hybrid_objects: Vec<HybridObjectEntry>,
}

/// A single entry in `nitro.json#autolinking`. Both `name` (the TS-side
/// HybridObject name) and `cxx_class` (the C++ implementation class name)
/// are derived from the uniffi metadata; the project-level template
/// composes them with the project's C++ namespace.
///
/// `cxx_namespace` is the per-uniffi-namespace component of the fully
/// qualified C++ name — every emitted impl class lives in
/// `margelo::nitro::<cxx_namespace>`. The `register_natives.cpp` emitter
/// needs it to spell each constructor with its full path, because one
/// cdylib can contain HybridObjects from multiple uniffi namespaces.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HybridObjectEntry {
    pub name: String,
    pub cxx_class: String,
    pub cxx_namespace: String,
    pub kind: HybridObjectKind,
}

/// Drive the full Nitro emission for a project. Iterates the
/// pipeline-loaded `general::Root`, building one [`NitroModule`] per
/// namespace, and writes:
///
/// * `.nitro.ts` (the spec)
/// * `.ts` (the re-export module)
/// * `Hybrid*.hpp/.cpp` (impl classes for the namespace API + each
///   interface)
/// * `<namespace>_codecs.hpp` (record / enum codecs)
///
/// Returns the collected [`NitroEmission`] so the caller can finish
/// the project-level scaffolding (nitro.json manifest etc.).
pub fn generate_all(
    root: &general::Root,
    ts_dir: &Utf8Path,
    cpp_dir: &Utf8Path,
) -> Result<NitroEmission> {
    let mut modules = Vec::new();
    let mut hybrid_objects: BTreeSet<HybridObjectEntry> = BTreeSet::new();

    // Register every foreign-implementable callback trait across all
    // namespaces *before* lowering any module, so a cross-crate use site
    // whose `Type::Interface.imp` was downgraded to `Trait` by a `typedef
    // trait` import is still recognized as a callback. The definition side
    // always carries the authoritative `imp == CallbackTrait`.
    model::clear_callback_traits();
    for (_, namespace) in &root.namespaces {
        for td in &namespace.type_definitions {
            if let general::TypeDefinition::Interface(iface) = td {
                if matches!(iface.imp, general::ObjectImpl::CallbackTrait) {
                    model::register_callback_trait(&namespace.name, &iface.name);
                }
            }
        }
    }

    for (name, namespace) in &root.namespaces {
        let nitro_module = NitroModule::from_general(namespace)?;
        let module = ModuleMetadata::new(name);

        // TS: spec + re-export module.
        ts::write_spec(ts_dir, &nitro_module)?;
        ts::write_reexport_module(ts_dir, &nitro_module, &module)?;

        // C++: per-interface + namespace API impl classes + codecs +
        // per-callback-interface trampolines.
        cpp::write_namespace_api(cpp_dir, &nitro_module)?;
        for iface in &nitro_module.interfaces {
            cpp::write_interface(cpp_dir, &nitro_module, iface)?;
        }
        for cb in &nitro_module.callback_interfaces {
            cpp::write_callback_interface(cpp_dir, &nitro_module, cb)?;
        }
        // Per-record / per-enum struct + JSIConverter headers (the
        // single source of truth — replaces Nitrogen's struct/converter
        // emission), then the RustBuffer codecs that build on them.
        for record in &nitro_module.records {
            cpp::write_record(cpp_dir, &nitro_module, record)?;
        }
        for en in &nitro_module.enums {
            cpp::write_enum(cpp_dir, &nitro_module, en)?;
        }
        cpp::write_codecs(cpp_dir, &nitro_module)?;

        // Collect autolinking entries.
        for entry in nitro_module.autolinking_entries() {
            hybrid_objects.insert(entry);
        }

        modules.push(module);
    }

    let hybrid_objects: Vec<HybridObjectEntry> = hybrid_objects.into_iter().collect();

    // Single project-level `register_natives.cpp` — the host Nitro
    // test runner `dlopen`s the cdylib and looks up
    // `extern "C" void registerNatives(jsi::Runtime&)`. Mobile builds
    // rely on Android `JNI_OnLoad` / iOS `+ load` autolinking instead,
    // but emitting unconditionally keeps the CMakeLists source list
    // invariant across platforms.
    cpp::write_register_natives(cpp_dir, &hybrid_objects)?;

    // Leave the thread-local clean for the next generate in this thread (the
    // next run also clears up-front, so a `?` early-return above is harmless).
    model::clear_callback_traits();

    Ok(NitroEmission {
        modules,
        hybrid_objects,
    })
}

// Convenience accessor for callers that own a fully populated
// `NitroEmission` and just need the autolinking entries as a sorted slice.
impl NitroEmission {
    // Part of the public emission surface; not yet exercised in-tree (the
    // autolinking entries are consumed via the template render path today).
    #[allow(dead_code)]
    pub fn hybrid_objects(&self) -> &[HybridObjectEntry] {
        &self.hybrid_objects
    }
}

impl PartialOrd for HybridObjectEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HybridObjectEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}
