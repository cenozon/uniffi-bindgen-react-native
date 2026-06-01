/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
//! IR for the Nitro codegen.
//!
//! Independent of `gen_typescript`'s IR because the surface we emit is
//! fundamentally different — every uniffi method becomes a typed Nitro
//! HybridObject method, not a host-object property keyed by
//! `ubrn_<symbol>`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, Result};
use heck::{ToLowerCamelCase, ToUpperCamelCase};

use uniffi_bindgen::pipeline::general;

use super::HybridObjectEntry;
use crate::bindings::gen_typescript::Config as TsConfig;

thread_local! {
    /// `(namespace, name)` of every foreign-implementable callback trait —
    /// proc-macro `#[uniffi::export(with_foreign)]` traits AND UDL
    /// `[Trait, WithForeign]` traits — across *all* namespaces in the current
    /// generate. Built from the authoritative type *definitions* (which always
    /// carry `imp == CallbackTrait`) so that a use-site `Type::Interface` whose
    /// `imp` was downgraded to `Trait` by a cross-crate `typedef trait` import
    /// is still recognized as a callback. See [`NitroType::from_type`]'s
    /// `Type::Interface` arm.
    ///
    /// Empty when generation hasn't registered anything (e.g. a unit test that
    /// builds a `NitroType` directly), which yields the plain-interface
    /// behavior — the correct default for a non-foreign `Trait`.
    static CALLBACK_TRAITS: RefCell<BTreeSet<(String, String)>> = const { RefCell::new(BTreeSet::new()) };

    /// `(namespace, name)` → the interface's `uniffi_<crate>_fn_free_<obj>`
    /// symbol, for every uniffi `interface` / `Object` AND every
    /// `with_foreign` trait (whose proxy half is also handle-bearing) across
    /// *all* namespaces in the current generate. Built from the authoritative
    /// type *definitions* (which carry `ffi_func_free`), so a USE-SITE
    /// `Type::Interface` / `Type::CallbackInterface` — which has only
    /// `(namespace, name)`, never the symbol — can recover its free symbol for
    /// the exception-safe handle guard (audit bug #24/#27). See
    /// [`NitroType::from_type`]'s `Interface` arm and [`NitroType::free_symbol`].
    ///
    /// Empty when generation hasn't registered anything (e.g. a unit test that
    /// builds a `NitroType` directly), which yields a `None` free symbol — the
    /// guard then degrades to the pre-existing bare-local lowering.
    static INTERFACE_FREE_SYMBOLS: RefCell<BTreeMap<(String, String), String>> = const { RefCell::new(BTreeMap::new()) };
}

/// Record `(namespace, name)` as a foreign-implementable callback trait for the
/// duration of the current generate. Called once per such definition before any
/// module is lowered (see `gen_nitro::generate_all`). Idempotent.
pub fn register_callback_trait(namespace: &str, name: &str) {
    CALLBACK_TRAITS.with(|set| {
        set.borrow_mut()
            .insert((namespace.to_string(), name.to_string()));
    });
}

/// Record an interface's (or `with_foreign` trait proxy's) `fn_free_<obj>`
/// symbol so a use-site `NitroType::Interface` / `CallbackInterface` can
/// recover it for the RAII handle guard. Called once per definition before any
/// module is lowered (see `gen_nitro::generate_all`). Idempotent.
pub fn register_interface_free_symbol(namespace: &str, name: &str, free_symbol: &str) {
    INTERFACE_FREE_SYMBOLS.with(|map| {
        map.borrow_mut().insert(
            (namespace.to_string(), name.to_upper_camel_case()),
            free_symbol.to_string(),
        );
    });
}

/// Drop every registered callback trait. Called at the end of a generate so a
/// later run in the same thread starts clean.
pub fn clear_callback_traits() {
    CALLBACK_TRAITS.with(|set| set.borrow_mut().clear());
    INTERFACE_FREE_SYMBOLS.with(|map| map.borrow_mut().clear());
}

fn is_registered_callback_trait(namespace: &str, name: &str) -> bool {
    CALLBACK_TRAITS.with(|set| {
        set.borrow()
            .contains(&(namespace.to_string(), name.to_string()))
    })
}

/// Look up the registered `fn_free_<obj>` symbol for an interface / callback
/// proxy by `(namespace, UpperCamelName)`. `None` when nothing was registered
/// (a direct unit-test `NitroType`, or a type whose definition lives outside
/// this generate) — the guard then degrades to the bare-local lowering.
fn registered_interface_free_symbol(namespace: &str, name: &str) -> Option<String> {
    INTERFACE_FREE_SYMBOLS.with(|map| {
        map.borrow()
            .get(&(namespace.to_string(), name.to_upper_camel_case()))
            .cloned()
    })
}

/// What flavor of HybridObject the entry corresponds to. Currently
/// purely informational — both kinds register under `language: c++` in
/// `nitro.json` — but downstream tooling can branch on this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HybridObjectKind {
    /// The namespace-level `<Namespace>Api` HybridObject. Aggregates
    /// the namespace's top-level functions and constructors.
    NamespaceApi,
    /// One per uniffi `interface`/`Object` definition.
    Interface,
    /// One per uniffi `callback interface`. The C++ vtable trampolines
    /// dispatch into instances created on the JS side.
    Callback,
}

/// A uniffi namespace, transformed into the shape the Nitro backend
/// emits. One `NitroModule` produces one set of files.
pub struct NitroModule {
    pub namespace: String,
    pub crate_name: String,
    pub namespace_camel: String,

    /// Top-level uniffi functions. Become methods on the namespace API
    /// HybridObject.
    pub functions: Vec<NitroFunction>,

    /// Uniffi interfaces / objects. Each becomes its own HybridObject.
    pub interfaces: Vec<NitroInterface>,

    /// Uniffi callback interfaces. Each becomes a TS interface in the
    /// spec + a C++ vtable trampoline file that registers with Rust on
    /// first use.
    pub callback_interfaces: Vec<NitroCallbackInterface>,

    /// Uniffi `dictionary` / Rust struct records. Each becomes a Nitro
    /// struct (auto-converted to JSI via Nitrogen's `JSIConverter` from
    /// the `.nitro.ts` spec) plus a pair of `lift_<Record>` /
    /// `lower_<Record>` codec functions in `<namespace>_codecs.hpp`
    /// that round-trip the struct through `RustBufferReader` /
    /// `RustBufferWriter`.
    pub records: Vec<NitroRecord>,

    /// Uniffi enums. Flat enums (no associated data) surface as TS
    /// string-unions + `i32` ordinal wire format. Tagged enums surface
    /// as TS discriminated unions; their codec is a future scope.
    pub enums: Vec<NitroEnum>,

    /// Uniffi error enums (`EnumShape::Error`). Become C++ exception
    /// classes (`<Name>Error : std::runtime_error`) plus per-variant
    /// `lift_<Name>Error(RustBuffer)` decoders. Emitted into
    /// `<namespace>_codecs.hpp` and the `.nitro.ts` spec.
    pub errors: Vec<NitroError>,

    /// Uniffi `custom` types (newtypes). Each becomes a nominal `export type
    /// <Name> = <inner>` alias in the consumer module plus, when the per-crate
    /// `uniffi.toml` configures `intoCustom` / `fromCustom`, the conversion
    /// wrappers the plain-TS surface applies. The wire form is the inner
    /// builtin's (see [`NitroType::Custom`]), so customs contribute no codec /
    /// C++ files of their own — this collection drives only the TS surface.
    /// See audit bug #17.
    pub customs: Vec<NitroCustom>,

    /// Per-namespace `ffi_<crate>_rustbuffer_*` symbol names. Captured
    /// straight from the pipeline so we always agree with whatever
    /// `uniffi_meta` decided to name them.
    pub rustbuffer_alloc: String,
    pub rustbuffer_free: String,
    pub rustbuffer_reserve: String,
}

impl NitroModule {
    pub fn from_general(namespace: &general::Namespace) -> Result<Self> {
        let crate_name = namespace.crate_name.clone();
        let ns_name = namespace.name.clone();
        let namespace_camel = ns_name.to_upper_camel_case();

        // The per-crate `uniffi.toml` `[bindings.typescript]` config (reusing
        // the same parse cli.rs `extract_ts_config` does), so a configured
        // custom-type `intoCustom`/`fromCustom` conversion lands on the
        // matching `NitroCustom`. A missing / empty config yields the default
        // (no configured conversions — plain nominal aliases). See bug #17.
        let ts_config = extract_nitro_ts_config(namespace)?;

        let mut functions = Vec::new();
        for func in &namespace.functions {
            // Skip top-level functions whose signature touches a uniffi
            // type the Nitro backend doesn't yet emit (e.g. an Interface
            // argument or return that ships its handle via the C ABI).
            // Emitting a partial namespace API is strictly more useful
            // than failing the whole module — the un-emitted method is
            // still accessible via the legacy JSI host-object path and
            // can be wired into Nitro once its type support lands.
            match NitroFunction::from_function(func) {
                Ok(f) => functions.push(f),
                Err(e) => eprintln!("nitro: skipping function `{}`: {e}", func.name),
            }
        }

        let mut interfaces = Vec::new();
        let mut callback_interfaces = Vec::new();
        let mut records = Vec::new();
        let mut enums = Vec::new();
        let mut errors = Vec::new();
        let mut customs = Vec::new();
        for td in &namespace.type_definitions {
            match td {
                general::TypeDefinition::Interface(iface) => {
                    // A `#[uniffi::export(with_foreign)]` trait arrives here as
                    // an `Interface` whose `imp` is `CallbackTrait` and which
                    // carries a `vtable` — it is foreign-implementable. uniffi
                    // ALSO emits Rust-callable method FFI symbols + clone/free
                    // for it (it's a real `Metadata::Object`), so such a trait
                    // is BOTH a callback (vtable trampolines, so JS impls can be
                    // handed to Rust) AND an interface-style proxy (so an
                    // `Arc<dyn Trait>` Rust returns can be wrapped and dispatched
                    // back into Rust). Emit the callback vtable from here; the
                    // proxy `Hybrid<Name>` surface is folded into the same
                    // callback class (see `NitroCallbackInterface`). A plain
                    // `interface` / trait object (`Struct` / `Trait` impl) stays
                    // an ordinary interface.
                    if matches!(iface.imp, general::ObjectImpl::CallbackTrait) {
                        callback_interfaces
                            .push(NitroCallbackInterface::from_foreign_trait(iface)?);
                    } else {
                        interfaces.push(NitroInterface::from_general(iface)?);
                    }
                }
                general::TypeDefinition::CallbackInterface(cb) => {
                    callback_interfaces.push(NitroCallbackInterface::from_general(cb)?);
                }
                general::TypeDefinition::Record(record) => {
                    records.push(NitroRecord::from_general(record)?);
                }
                general::TypeDefinition::Enum(en) => {
                    // uniffi splits the same Enum AST into "data enums" and
                    // "error enums" via the `shape` discriminant. We surface
                    // them into different IR collections because the codegen
                    // shapes diverge — errors become C++ exception classes
                    // + per-error `lift_*Error` decoders; data enums become
                    // value types with `lift_*` / `lower_*` codecs.
                    match en.shape {
                        general::EnumShape::Error { .. } => {
                            errors.push(NitroError::from_general(en)?);
                            // A uniffi error enum can also be used as a plain
                            // value (field / argument / return) — e.g.
                            // `fn get_tuple(t: Option<MyError>) -> MyError`.
                            // Emit its value representation too (the `<Name>.hpp`
                            // enum + `JSIConverter<Name>` + `lift_/lower_<Name>`
                            // codec), so value uses resolve. The thrown form is
                            // a distinct `<Name>Error` exception class
                            // (TS `<Name>Variant`), so the two never collide.
                            enums.push(NitroEnum::from_general(en)?);
                        }
                        general::EnumShape::Enum => {
                            enums.push(NitroEnum::from_general(en)?);
                        }
                    }
                }
                general::TypeDefinition::Custom(custom) => {
                    customs.push(NitroCustom::from_general(custom, &ts_config)?);
                }
                _ => {}
            }
        }

        let mut module = Self {
            namespace: ns_name,
            crate_name,
            namespace_camel,
            functions,
            interfaces,
            callback_interfaces,
            records,
            enums,
            errors,
            customs,
            rustbuffer_alloc: namespace.ffi_rustbuffer_alloc.0.clone(),
            rustbuffer_free: namespace.ffi_rustbuffer_free.0.clone(),
            rustbuffer_reserve: namespace.ffi_rustbuffer_reserve.0.clone(),
        };
        module.resolve_cycles();
        Ok(module)
    }

    /// Detect record/enum header `#include` cycles and record each cyclic
    /// type's SCC partners on the type, so the templates can switch to the
    /// cycle-safe layout.
    ///
    /// ## Why this is needed
    ///
    /// A record/enum `<Name>.hpp` both *defines* the struct and *defines* its
    /// `JSIConverter`. A record holding another record/enum by value (or an
    /// enum constructing a payload that embeds one) needs that type
    /// **complete**, so `<Name>.hpp` `#include`s the dependency. When two such
    /// headers reference each other (e.g. `DbValue` holds `vector<DbMapEntry>`
    /// and `DbMapEntry` holds `DbValue` by value), the mutual `#include` +
    /// `#pragma once` means whichever header the translation unit enters first
    /// hits the other before the first type is declared — the by-value field
    /// then names an incomplete type and the TU fails to compile. (Pure
    /// self-recursion — `Tree` holding `vector<Tree>` — is fine: a `vector` of
    /// an incomplete element type is legal, and the single header completes the
    /// type before its own converter.)
    ///
    /// ## What "cycle" means here
    ///
    /// We build a directed graph over this namespace's records + enums where an
    /// edge `X → Y` means *X's header would `#include "Y.hpp"`* (X references
    /// the record/enum Y in a field — by value or through
    /// `vector`/`optional`/`map`; interfaces/callbacks are excluded as they
    /// only ever need a forward declaration). Self-edges are dropped. A
    /// strongly-connected component of size ≥ 2 is a genuine header cycle; each
    /// of its members gets the cycle-safe layout. (Cross-namespace cycles can't
    /// arise — uniffi records/enums only reference types in their own
    /// namespace's metadata graph — so a single-namespace SCC pass suffices.)
    fn resolve_cycles(&mut self) {
        // node key = UpperCamel type name (unique within a namespace).
        let ns = self.namespace.clone();
        let mut nodes: BTreeSet<String> = BTreeSet::new();
        for r in &self.records {
            nodes.insert(r.ts_name.clone());
        }
        for e in &self.enums {
            nodes.insert(e.ts_name.clone());
        }

        // adjacency: X -> set of referenced same-namespace record/enum names
        // (excluding self).
        let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut add_edges = |from: &str, refs: &BTreeSet<(String, String)>| {
            let e = adj.entry(from.to_string()).or_default();
            for (rns, rname) in refs {
                if rns == &ns && nodes.contains(rname) && rname != from {
                    e.insert(rname.clone());
                }
            }
        };
        // `by_value_refs[X]` = the same-namespace record/enum names X holds by
        // value (a direct field, not wrapped) — these must be complete at X's
        // struct definition. Drives the per-partner `by_value` flag below.
        let mut by_value_refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let record_by_value = |from: &str, ty: &NitroType, bv: &mut BTreeSet<String>| {
            if let Some((rns, rname)) = ty.by_value_type() {
                if rns == ns && nodes.contains(&rname) && rname != from {
                    bv.insert(rname);
                }
            }
        };
        for r in &self.records {
            let mut refs = BTreeSet::new();
            let mut bv = BTreeSet::new();
            for f in &r.fields {
                f.ty.referenced_value_types(&mut refs);
                record_by_value(&r.ts_name, &f.ty, &mut bv);
            }
            add_edges(&r.ts_name, &refs);
            by_value_refs.insert(r.ts_name.clone(), bv);
        }
        for e in &self.enums {
            let mut refs = BTreeSet::new();
            let mut bv = BTreeSet::new();
            for f in e.variants.iter().flat_map(|v| v.fields.iter()) {
                f.ty.referenced_value_types(&mut refs);
                record_by_value(&e.ts_name, &f.ty, &mut bv);
            }
            add_edges(&e.ts_name, &refs);
            by_value_refs.insert(e.ts_name.clone(), bv);
        }

        let sccs = strongly_connected_components(&nodes, &adj);
        // Map each cyclic node -> the sorted partner spellings (excl. self).
        let mut partners_of: BTreeMap<String, Vec<CycleMember>> = BTreeMap::new();
        for scc in &sccs {
            if scc.len() < 2 {
                continue;
            }
            for member in scc {
                let owner_bv = by_value_refs.get(member).cloned().unwrap_or_default();
                let partners: Vec<CycleMember> = scc
                    .iter()
                    .filter(|other| *other != member)
                    .map(|name| CycleMember {
                        name: name.clone(),
                        header: format!("{name}.hpp"),
                        conv_header: format!("{name}.conv.hpp"),
                        struct_sentinel: format!("UBRN_CYC_{ns}_{name}_STRUCT"),
                        by_value: owner_bv.contains(name),
                    })
                    .collect();
                partners_of.insert(member.clone(), partners);
            }
        }

        for r in &mut self.records {
            if let Some(p) = partners_of.remove(&r.ts_name) {
                r.cycle_partners = p;
            }
        }
        for e in &mut self.enums {
            if let Some(p) = partners_of.remove(&e.ts_name) {
                e.cycle_partners = p;
            }
        }
    }

    pub fn namespace_api_ts_name(&self) -> String {
        format!("{}Api", self.namespace_camel)
    }

    /// The lowerCamelCase name of the namespace-singleton accessor the consumer
    /// module exports (e.g. `extTypesCustom()` for namespace `ext_types_custom`),
    /// so it reads consistently with its camelCase function / type siblings
    /// rather than the raw snake_case namespace name. The registry string
    /// literal (`'<Ns>Api'`) is unaffected — only the JS accessor identifier
    /// changes. See audit bug #18.
    pub fn namespace_accessor(&self) -> String {
        self.namespace.to_lower_camel_case()
    }

    pub fn namespace_api_cxx_class(&self) -> String {
        format!("Hybrid{}Api", self.namespace_camel)
    }

    pub fn nitro_ts_filename(&self) -> String {
        format!("{}.nitro.ts", self.namespace_camel)
    }

    pub fn reexport_ts_filename(&self) -> String {
        format!("{}.ts", self.namespace)
    }

    pub fn codecs_header_filename(&self) -> String {
        format!("{}_codecs.hpp", self.namespace)
    }

    /// The `<foreign_ns>_codecs.hpp` headers this namespace's emission must
    /// `#include` so that cross-namespace record / enum codec calls
    /// (`::margelo::nitro::<foreign_ns>::lower_/lift_/write_/read_<Name>`)
    /// resolve. Walks every type the module references — record / enum
    /// fields, function + method + callback args and returns — and collects
    /// each foreign namespace exactly once. Cross-namespace generation lays
    /// all namespaces' files into one output directory (see
    /// [`super::generate_all`]), so the foreign header is a plain
    /// same-directory include.
    ///
    /// Emitted into `codecs.hpp`; the impl files (`interface.cpp`,
    /// `namespace_api.cpp`, `callback.cpp`) already include this namespace's
    /// `codecs.hpp`, so they inherit the foreign codecs transitively.
    pub fn foreign_codec_headers(&self) -> Vec<String> {
        let mut namespaces: BTreeSet<String> = BTreeSet::new();
        let ns = self.namespace.as_str();

        for record in &self.records {
            for field in &record.fields {
                field.ty.foreign_codec_namespaces(ns, &mut namespaces);
            }
        }
        for en in &self.enums {
            for field in en.variants.iter().flat_map(|v| v.fields.iter()) {
                field.ty.foreign_codec_namespaces(ns, &mut namespaces);
            }
        }
        for err in &self.errors {
            for field in err.variants.iter().flat_map(|v| v.fields.iter()) {
                field.ty.foreign_codec_namespaces(ns, &mut namespaces);
            }
        }
        for func in &self.functions {
            func.collect_foreign_codec_namespaces(ns, &mut namespaces);
        }
        for iface in &self.interfaces {
            for m in iface.constructors.iter().chain(iface.methods.iter()) {
                m.collect_foreign_codec_namespaces(ns, &mut namespaces);
            }
        }
        for cb in &self.callback_interfaces {
            for m in &cb.methods {
                for arg in &m.args {
                    arg.ty.foreign_codec_namespaces(ns, &mut namespaces);
                }
                if let ReturnKind::Value(t) = &m.return_kind {
                    t.foreign_codec_namespaces(ns, &mut namespaces);
                }
            }
        }

        namespaces
            .into_iter()
            .map(|ns| format!("{ns}_codecs.hpp"))
            .collect()
    }

    /// Cross-namespace TS type imports the `.nitro.ts` spec must emit, one
    /// entry per foreign namespace, each carrying the module path
    /// (`./<ForeignNamespaceCamel>.nitro`) and the sorted set of type names
    /// referenced from that module. Walks every surface that can name a
    /// foreign type: record fields, enum / error variant fields, every
    /// function / interface-method / callback-method arg + return, and the
    /// namespace API methods (constructor factories).
    ///
    /// The TS spec has no `export *` re-export glue between sibling
    /// `.nitro.ts` files, so each referenced foreign type must be imported
    /// explicitly by its own module — mirroring how
    /// [`Self::foreign_codec_headers`] collects the foreign C++ codec
    /// `#include`s. Deduped + sorted (BTreeMap / BTreeSet) for stable
    /// output.
    pub fn foreign_ts_type_imports(&self) -> Vec<ForeignTsImport> {
        let mut by_ns: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let ns = self.namespace.as_str();

        let mut collect = |ty: &NitroType| ty.foreign_ts_type_refs(ns, &mut by_ns);

        for record in &self.records {
            for field in &record.fields {
                collect(&field.ty);
            }
        }
        for en in &self.enums {
            for field in en.variants.iter().flat_map(|v| v.fields.iter()) {
                collect(&field.ty);
            }
        }
        for err in &self.errors {
            for field in err.variants.iter().flat_map(|v| v.fields.iter()) {
                collect(&field.ty);
            }
        }
        // A custom type can wrap a FOREIGN inner (e.g. `NestedExternalGuid =
        // Guid` where `Guid` lives in another namespace), and the alias RHS
        // names that inner — so its foreign refs must be imported too.
        for c in &self.customs {
            collect(&c.inner);
        }
        // Namespace API methods cover both top-level functions and the
        // constructor factories (factories return / take foreign types too).
        for func in self.api_methods() {
            for arg in &func.args {
                collect(&arg.ty);
            }
            if let ReturnKind::Value(t) = &func.return_kind {
                collect(t);
            }
        }
        for iface in &self.interfaces {
            for m in iface.methods.iter() {
                for arg in &m.args {
                    collect(&arg.ty);
                }
                if let ReturnKind::Value(t) = &m.return_kind {
                    collect(t);
                }
            }
        }
        for cb in &self.callback_interfaces {
            for m in &cb.methods {
                for arg in &m.args {
                    collect(&arg.ty);
                }
                if let ReturnKind::Value(t) = &m.return_kind {
                    collect(t);
                }
            }
        }

        by_ns
            .into_iter()
            .map(|(foreign_ns, names)| ForeignTsImport {
                module_path: format!("./{}.nitro", foreign_ns.to_upper_camel_case()),
                type_names: names.into_iter().collect(),
            })
            .collect()
    }

    /// Every method the namespace API HybridObject exposes: the namespace's
    /// top-level functions, followed by one factory per non-primary
    /// interface constructor. A constructor factory is just a function whose
    /// return is the interface handle, so it shares the namespace-API method
    /// emission (lowering / async / fallible / lift) wholesale — the
    /// templates iterate this single list rather than `functions` so the
    /// bodies aren't duplicated.
    pub fn api_methods(&self) -> Vec<&NitroFunction> {
        let mut out: Vec<&NitroFunction> = self.functions.iter().collect();
        for iface in &self.interfaces {
            out.extend(iface.factories.iter());
        }
        out
    }

    /// The namespace's top-level uniffi functions ONLY — no constructor
    /// factories folded in. The consumer-module TS surface emits these as the
    /// flat `export function` forwarders; the constructor factories are NOT
    /// surfaced as free functions on the consumer side (they are reached only
    /// through each interface's runtime class — `new <Iface>(...)` /
    /// `<Iface>.<ctor>(...)`), so the TS class template iterates per-interface
    /// `factories` rather than this list. Contrast [`Self::api_methods`], which
    /// DOES fold the factories in because the C++ `<Ns>Api` HybridObject still
    /// exposes every factory as a method (the class statics delegate to it).
    pub fn top_level_functions(&self) -> &[NitroFunction] {
        &self.functions
    }

    /// `true` when the namespace-API HybridObject exposes at least one async
    /// method (a top-level async function or an async constructor factory).
    /// Gates emission of the non-spec `__uniffiBeginAbortable()` /
    /// `__uniffiAbort()` cancel hooks on the namespace-API HybridObject.
    pub fn has_async_api_methods(&self) -> bool {
        self.api_methods().iter().any(|f| f.is_async)
    }

    /// The configured custom-type conversion owned by THIS namespace for the
    /// custom type named `name` (its UpperCamelCase TS name), if the per-crate
    /// `uniffi.toml` declared one. Cross-namespace customs are presented as a
    /// plain `import type` alias and converted (if at all) by their owning
    /// namespace's wrapper, so only same-namespace customs are looked up here.
    /// See audit bug #17.
    fn custom_conversion(&self, name: &str) -> Option<&NitroCustomConversion> {
        self.customs
            .iter()
            .find(|c| c.ts_name == name)
            .and_then(|c| c.conversion.as_ref())
    }

    /// The deduped `(import-name, module)` pairs every configured custom
    /// conversion in this namespace needs in scope (e.g. `URL` from
    /// `@/converters`). Emitted as named imports at the top of the consumer
    /// wrapper so the `intoCustom` / `fromCustom` expressions resolve.
    pub fn custom_conversion_imports(&self) -> Vec<TsNamedImport> {
        let mut by_module: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for c in &self.customs {
            if let Some(conv) = c.conversion.as_ref() {
                for (name, module) in &conv.imports {
                    by_module
                        .entry(module.clone())
                        .or_default()
                        .insert(name.clone());
                }
            }
        }
        by_module
            .into_iter()
            .map(|(module_path, names)| TsNamedImport {
                module_path,
                type_names: names.into_iter().collect(),
            })
            .collect()
    }

    /// The expression a free-function / method wrapper passes into the Nitro
    /// singleton for argument `arg`. When the arg is a same-namespace custom
    /// type with a configured conversion, the presented (`typeName`) value is
    /// lowered to the inner builtin via `fromCustom`; an `Optional<custom>` is
    /// lowered element-wise (preserving `undefined`). Every other arg (plain
    /// builtin, unconfigured newtype, record, enum, interface, …) passes
    /// straight through by name. See audit bug #17.
    pub fn lower_call_arg(&self, arg: &NitroArg) -> String {
        match &arg.ty {
            NitroType::Custom { name, .. } => match self.custom_conversion(name) {
                Some(conv) => conv.lower(&arg.ts_name),
                None => arg.ts_name.clone(),
            },
            NitroType::Optional(inner) => match inner.as_ref() {
                NitroType::Custom { name, .. } => match self.custom_conversion(name) {
                    Some(conv) => format!(
                        "({0} === undefined ? undefined : {1})",
                        arg.ts_name,
                        conv.lower(&arg.ts_name)
                    ),
                    None => arg.ts_name.clone(),
                },
                _ => arg.ts_name.clone(),
            },
            _ => arg.ts_name.clone(),
        }
    }

    /// The complete consumer-wrapper `return` RHS for a top-level free
    /// function: the Nitro singleton call with each argument lowered through
    /// its custom conversion ([`Self::lower_call_arg`]), wrapped in the
    /// return-side custom lift ([`Self::lift_return_expr`]). When the function
    /// touches no configured custom type this collapses to the plain
    /// `<accessor>().<fn>(arg0, arg1)` pass-through. See audit bug #17.
    pub fn free_function_call(&self, func: &NitroFunction) -> String {
        let accessor = format!("{}()", self.namespace_accessor());
        self.free_function_call_on(func, &accessor)
    }

    /// Like [`Self::free_function_call`] but against a caller-supplied receiver
    /// expression (e.g. a bound `__api` local) rather than re-deriving the
    /// `<accessor>()` singleton call each time. The abortable async wrapper
    /// binds the singleton once so its `__uniffiBeginAbortable()` /
    /// `__uniffiAbort(token)` calls and the typed call all target the SAME
    /// instance (and the begin/typed-call pair stays synchronous, so the armed
    /// cancel token is consumed by exactly this call's kick-off).
    pub fn free_function_call_on(&self, func: &NitroFunction, receiver: &str) -> String {
        let args = func
            .args
            .iter()
            .map(|a| self.lower_call_arg(a))
            .collect::<Vec<_>>()
            .join(", ");
        let call = format!("{receiver}.{}({args})", func.ts_name);
        self.lift_return_expr(func, &call)
    }

    /// Given the `call_expr` that invokes the Nitro singleton (returning the
    /// inner-builtin value), render the consumer wrapper's `return` RHS. When
    /// the return is a same-namespace custom type with a configured conversion,
    /// the inner value is lifted to the presented (`typeName`) type via
    /// `intoCustom`; for an async function the lift is threaded through `.then`
    /// (the call already yields a `Promise`); an `Optional<custom>` is lifted
    /// element-wise. Every other return passes straight through. See bug #17.
    pub fn lift_return_expr(&self, func: &NitroFunction, call_expr: &str) -> String {
        let ReturnKind::Value(ty) = &func.return_kind else {
            return call_expr.to_string();
        };
        let Some((conv, optional)) = self.return_custom_conversion(ty) else {
            return call_expr.to_string();
        };
        // `__v` is the inner-builtin value the singleton produced.
        let lift_one = |v: &str| {
            if optional {
                format!("({0} === undefined ? undefined : {1})", v, conv.lift(v))
            } else {
                conv.lift(v)
            }
        };
        if func.is_async {
            format!("({call_expr}).then((__v) => {})", lift_one("__v"))
        } else {
            // Bind the call result once so the lift expression can reference it
            // (custom `intoCustom` templates may mention `{}` more than once).
            format!("((__v) => {})({call_expr})", lift_one("__v"))
        }
    }

    /// `(conversion, is_optional)` when `ty` is a same-namespace configured
    /// custom (directly or wrapped in a single `Optional`), else `None`.
    fn return_custom_conversion(&self, ty: &NitroType) -> Option<(&NitroCustomConversion, bool)> {
        match ty {
            NitroType::Custom { name, .. } => self.custom_conversion(name).map(|c| (c, false)),
            NitroType::Optional(inner) => match inner.as_ref() {
                NitroType::Custom { name, .. } => {
                    self.custom_conversion(name).map(|c| (c, true))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Per-type headers the namespace API's method declarations reference.
    /// Deduped + sorted so the emitted `#include` block is stable. Covers
    /// top-level functions *and* constructor factories — the latter return
    /// (and may take) interface / record / enum types whose headers the API
    /// impl must see.
    pub fn api_dependency_headers(&self) -> Vec<String> {
        dedup_headers(
            self.api_methods()
                .iter()
                .flat_map(|f| f.referenced_headers()),
            "",
        )
    }

    pub fn autolinking_entries(&self) -> Vec<HybridObjectEntry> {
        let mut entries = Vec::new();
        entries.push(HybridObjectEntry {
            name: self.namespace_api_ts_name(),
            cxx_class: self.namespace_api_cxx_class(),
            cxx_namespace: self.namespace.clone(),
            kind: HybridObjectKind::NamespaceApi,
        });
        for iface in &self.interfaces {
            entries.push(HybridObjectEntry {
                name: iface.ts_name.clone(),
                cxx_class: iface.cxx_class.clone(),
                cxx_namespace: self.namespace.clone(),
                kind: HybridObjectKind::Interface,
            });
        }
        for cb in &self.callback_interfaces {
            entries.push(HybridObjectEntry {
                name: cb.ts_name.clone(),
                cxx_class: cb.cxx_class.clone(),
                cxx_namespace: self.namespace.clone(),
                kind: HybridObjectKind::Callback,
            });
        }
        entries
    }
}

pub struct NitroFunction {
    pub ts_name: String,
    pub cxx_name: String,
    /// The uniffi-pipeline-derived C ABI symbol — never guess this; it's
    /// in `Callable.ffi_func`.
    pub uniffi_symbol: String,
    pub args: Vec<NitroArg>,
    pub return_kind: ReturnKind,
    pub is_async: bool,
    /// Typed error this function may throw. `None` means the method is
    /// declared infallible (any `RustCallStatusCode::Error` from the
    /// runtime is itself a programming error). When present the C++
    /// method body wraps `check_status` in a try-catch that decodes the
    /// status buffer via `lift_<Name>Error` and rethrows as the typed
    /// C++ exception class.
    pub throws: Option<NitroErrorRef>,
    /// FFI-symbol triple required for the async poll loop. Only set
    /// when `is_async == true`. The four `ffi_rust_future_*` names are
    /// per-return-FFI-type, so they're per-function rather than
    /// per-namespace.
    pub async_data: Option<NitroAsyncData>,
    /// For a constructor factory only: the JS static-method name the
    /// consumer-module interface class exposes for this constructor (the
    /// bare lowerCamel uniffi ctor name). `None` for the uniffi-primary
    /// `new` ctor — it maps to the class `constructor` (sync) or a static
    /// async factory, not a named static — and for plain top-level
    /// functions / methods (which are not class statics). `Some("fallibleNew")`
    /// etc. for every alternate / named constructor. Reserved-word-safe via
    /// [`ts_fn_name`]. The `cxx_name` / `ts_name` keep the
    /// `create<Iface>[<Ctor>]` factory spelling (the `.nitro.ts` spec +
    /// C++ HybridObject method the static delegates to).
    pub static_name: Option<String>,
    /// `true` when this factory is the uniffi-authoritative *primary*
    /// constructor (`CallableKind::Constructor { primary: true, .. }`, i.e.
    /// the `new` ctor) that landed in `factories` because it is NOT a
    /// drivable default (it takes args / is async / is fallible). The
    /// consumer-module class template routes it to the JS `constructor`
    /// (sync arg-taking / fallible-only) or a static promise-returning
    /// factory (async) rather than to a named static. `false` for alternate
    /// constructors and for non-constructor functions.
    pub is_primary_ctor: bool,
    /// Author docstring from the uniffi metadata, if any. Emitted as JSDoc on
    /// the function / method / constructor in the consumer surface. See audit
    /// bug #23.
    pub docstring: Option<String>,
}

/// Reference to a uniffi error type from a callable's `throws` slot.
/// Carries enough info to spell out the C++ exception class name and the
/// per-error `lift_<Name>Error` symbol in the generated method body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NitroErrorRef {
    /// UpperCamelCase TS name (also the unqualified C++ class name).
    pub ts_name: String,
    /// Owning uniffi namespace. When it differs from the throwing
    /// callable's namespace, the decoder lives in the *foreign*
    /// `<namespace>_codecs.hpp` and must be name-qualified.
    pub namespace: String,
}

impl NitroErrorRef {
    /// C++ exception class name. To disambiguate from a same-named data-enum
    /// value type that co-exists in the same TU (`struct ComplexError`), the
    /// exception is `<base>Exception` where `<base>` is `ts_name` with a
    /// trailing `Error` stripped — so `ComplexError` → `ComplexException` (not
    /// the old double-suffixed `ComplexErrorError`) and `RootError` →
    /// `RootException`. A name that doesn't end in `Error` simply gains the
    /// `Exception` suffix (`Arithmetic` → `ArithmeticException`). MUST agree
    /// byte-for-byte with [`NitroError::cxx_class`] (the definition side). See
    /// audit bug #28.
    pub fn cxx_class(&self) -> String {
        error_cxx_class(&self.ts_name)
    }

    /// Free-function decoder name in the owning namespace's
    /// `<namespace>_codecs.hpp`, qualified with the foreign namespace when
    /// the error is defined outside `current_ns` (so a cross-crate throws
    /// resolves against the included foreign codecs header). Derived from
    /// [`Self::cxx_class`] so the lifter and the class it returns stay coupled.
    pub fn lift_fn(&self, current_ns: &str) -> String {
        let prefix = if self.namespace == current_ns {
            String::new()
        } else {
            format!("::margelo::nitro::{}::", self.namespace)
        };
        format!("{prefix}lift_{}", self.cxx_class())
    }

    // NOTE: no `lower_fn` (the `code=1` typed-error encode path) — encoding a
    // JS-thrown typed error back into a uniffi `RustBuffer` for a sync callback
    // is Nitro-impossible (audit E1: a JS throw reaches C++ only as a
    // string-only `jsi::JSError`, never a typed payload). The sync callback
    // trampoline reports the reason string via `code=2`; typed identity is
    // recovered on the JS side through the `<Err>_Tags` message-prefix
    // discriminator. See audit bug #7.
}

/// The C++ exception class name for a uniffi error type whose UpperCamelCase TS
/// name is `ts_name`: strip a trailing `Error` and append `Exception`, so an
/// already-`Error`-suffixed name doesn't double up (`ComplexError` →
/// `ComplexException`) and a bare name still disambiguates from its value-type
/// twin (`Arithmetic` → `ArithmeticException`). Single source of truth shared
/// by [`NitroError::cxx_class`] and [`NitroErrorRef::cxx_class`]. See bug #28.
fn error_cxx_class(ts_name: &str) -> String {
    let base = ts_name.strip_suffix("Error").unwrap_or(ts_name);
    format!("{base}Exception")
}

/// FFI symbols required to drive the uniffi rust-future poll loop. All
/// four are derived from `Callable.async_data` in the pipeline (which is
/// `Some` exactly when `is_async == true`).
///
/// Each symbol is per-return-FFI-type (e.g. `ffi_<crate>_rust_future_complete_u8`
/// vs `..._rust_buffer`), so they're attached to each `NitroFunction`
/// rather than to the namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NitroAsyncData {
    pub poll_symbol: String,
    pub cancel_symbol: String,
    pub free_symbol: String,
    pub complete_symbol: String,
}

impl NitroAsyncData {
    fn from_general(data: &general::AsyncData) -> Self {
        Self {
            poll_symbol: data.ffi_rust_future_poll.0.clone(),
            cancel_symbol: data.ffi_rust_future_cancel.0.clone(),
            free_symbol: data.ffi_rust_future_free.0.clone(),
            complete_symbol: data.ffi_rust_future_complete.0.clone(),
        }
    }
}

impl NitroFunction {
    fn from_function(func: &general::Function) -> Result<Self> {
        let ts_name = ts_fn_name(&func.name);
        let cxx_name = sanitize_cxx_ident(&func.name.to_lower_camel_case());
        let uniffi_symbol = func.callable.ffi_func.0.clone();
        let mut args = Vec::new();
        for arg in &func.inputs {
            args.push(NitroArg::from_general(arg)?);
        }
        let return_kind = ReturnKind::from_general(func.return_type.as_ref())?;
        Ok(Self {
            ts_name,
            cxx_name,
            uniffi_symbol,
            args,
            return_kind,
            is_async: func.is_async,
            throws: throws_from(func.throws.as_ref()),
            async_data: func
                .callable
                .async_data
                .as_ref()
                .map(NitroAsyncData::from_general),
            static_name: None,
            is_primary_ctor: false,
            docstring: func.docstring.clone(),
        })
    }

    fn from_constructor(ctor: &general::Constructor) -> Result<Self> {
        let ts_name = ts_fn_name(&ctor.name);
        let cxx_name = sanitize_cxx_ident(&ctor.name.to_lower_camel_case());
        let uniffi_symbol = ctor.callable.ffi_func.0.clone();
        let mut args = Vec::new();
        for arg in &ctor.inputs {
            args.push(NitroArg::from_general(arg)?);
        }
        // The argless/sync/infallible "primary" constructor is wired into
        // the HybridObject default constructor (see
        // `NitroInterface::primary_constructor`), so it reports `Void` here —
        // the default-ctor path reads only the symbol + args, never a return
        // type. Non-primary constructors are surfaced as factory methods via
        // `from_constructor_factory`, which sets the interface as the return.
        Ok(Self {
            ts_name,
            cxx_name,
            uniffi_symbol,
            args,
            return_kind: ReturnKind::Void,
            is_async: ctor.is_async,
            throws: throws_from(ctor.throws.as_ref()),
            async_data: ctor
                .callable
                .async_data
                .as_ref()
                .map(NitroAsyncData::from_general),
            static_name: None,
            is_primary_ctor: false,
            docstring: ctor.docstring.clone(),
        })
    }

    /// Build a *factory* `NitroFunction` for a non-primary constructor. The
    /// surface is `create<Interface>[<CtorName>](<args>) -> <Interface>`:
    /// a method on the namespace API HybridObject whose body lowers the
    /// args, calls the uniffi constructor symbol (which returns an owned
    /// `uint64_t` Arc handle), and wraps it in `std::make_shared<Hybrid<Name>>`.
    ///
    /// `ret` is the interface's own [`NitroType::Interface`] — reusing the
    /// generic `ReturnKind::Value(Interface)` lift means the async / fallible
    /// paths in `namespace_api.cpp` need no special-casing: a constructor is
    /// just a function whose return is the interface handle. The returned
    /// handle is owned (uniffi `Arc::into_raw`), so the lift wraps it
    /// directly with no extra clone.
    ///
    /// The factory's `ts_name`/`cxx_name` keep the `create<Iface>[<Ctor>]`
    /// spelling (the `.nitro.ts` spec + C++ HybridObject method name). The
    /// consumer-module interface *class* surface is driven by the separate
    /// [`Self::static_name`] / [`Self::is_primary_ctor`] fields set here:
    /// `is_primary` is the uniffi-authoritative
    /// `CallableKind::Constructor { primary: true, .. }` flag, and
    /// `static_name` is the bare lowerCamel ctor name the class exposes as a
    /// static (`None` for the primary `new`, which maps to the JS
    /// `constructor` / a static async factory).
    fn from_constructor_factory(
        ctor: &general::Constructor,
        iface_ts_name: &str,
        ret: NitroType,
    ) -> Result<Self> {
        let ctor_name = ctor.name.to_upper_camel_case();
        // `new` is the conventional sole/primary constructor name — drop it
        // from the factory name so the common shape reads `create<Interface>`.
        // Any other named constructor disambiguates with its own suffix.
        let factory_base = if ctor_name == "New" {
            format!("create{iface_ts_name}")
        } else {
            format!("create{iface_ts_name}{ctor_name}")
        };
        let ts_name = ts_fn_name(&factory_base);
        let cxx_name = sanitize_cxx_ident(&factory_base.to_lower_camel_case());
        let uniffi_symbol = ctor.callable.ffi_func.0.clone();
        let mut args = Vec::new();
        for arg in &ctor.inputs {
            args.push(NitroArg::from_general(arg)?);
        }
        // uniffi-authoritative primary flag (set by name: `new` -> primary),
        // independent of arity / async / throws — exactly what gen_typescript
        // reads (builders.rs `CallableKind::Constructor { primary: true, .. }`).
        let is_primary_ctor = matches!(
            ctor.callable.kind,
            general::CallableKind::Constructor { primary: true, .. }
        );
        // The class static the consumer module exposes for this ctor: the bare
        // lowerCamel uniffi ctor name (`fallibleNew`, `secondary`, ...).
        // The primary `new` ctor has no named static — it becomes the JS
        // `constructor` (sync) or a static async factory — so it is `None`.
        let static_name = if is_primary_ctor {
            None
        } else {
            Some(ts_fn_name(&ctor.name))
        };
        Ok(Self {
            ts_name,
            cxx_name,
            uniffi_symbol,
            args,
            return_kind: ReturnKind::Value(ret),
            is_async: ctor.is_async,
            throws: throws_from(ctor.throws.as_ref()),
            async_data: ctor
                .callable
                .async_data
                .as_ref()
                .map(NitroAsyncData::from_general),
            static_name,
            is_primary_ctor,
            docstring: ctor.docstring.clone(),
        })
    }

    fn from_method(method: &general::Method) -> Result<Self> {
        let ts_name = ts_fn_name(&method.name);
        let cxx_name = sanitize_cxx_ident(&method.name.to_lower_camel_case());
        let uniffi_symbol = method.callable.ffi_func.0.clone();
        let mut args = Vec::new();
        for arg in &method.inputs {
            args.push(NitroArg::from_general(arg)?);
        }
        let return_kind = ReturnKind::from_general(method.return_type.as_ref())?;
        Ok(Self {
            ts_name,
            cxx_name,
            uniffi_symbol,
            args,
            return_kind,
            is_async: method.is_async,
            throws: throws_from(method.throws.as_ref()),
            async_data: method
                .callable
                .async_data
                .as_ref()
                .map(NitroAsyncData::from_general),
            static_name: None,
            is_primary_ctor: false,
            docstring: method.docstring.clone(),
        })
    }
}

/// Extract a `NitroErrorRef` from a uniffi `throws` slot. Only `Enum`
/// types appear in the throws slot for the fixtures we currently care
/// about; non-enum throws are silently dropped (they'd need a separate
/// codec strategy and the upstream pipeline normally rejects them).
fn throws_from(ty: Option<&general::Type>) -> Option<NitroErrorRef> {
    match ty? {
        general::Type::Enum { name, namespace, .. } => Some(NitroErrorRef {
            ts_name: name.to_upper_camel_case(),
            namespace: namespace.clone(),
        }),
        _ => None,
    }
}

/// Reshape a uniffi object's `UniffiTraitMethods` into the emittable
/// [`NitroUniffiTrait`] list. Each present trait method is built as an ordinary
/// [`NitroFunction`] (reusing the FFI-symbol / arg-lowering / return-lift path),
/// then tagged with which Nitro surface it maps to. Mirrors gen_typescript's
/// `collect_uniffi_traits` (`api_module/builders.rs`): Display→`toString`,
/// Debug→`toDebugString`, Eq→`equals` (only `eq`, never `ne`), Hash→`hashCode`,
/// Ord→`compareTo`. A trait method that fails to parse is skipped (logged), so a
/// single unsupported shape never drops the whole interface. See parity item P2.
fn collect_uniffi_traits(tm: &general::UniffiTraitMethods) -> Vec<NitroUniffiTrait> {
    let mut traits = Vec::new();
    let build = |slot: &Option<general::Method>, label: &str| -> Option<NitroFunction> {
        let m = slot.as_ref()?;
        match NitroFunction::from_method(m) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("nitro: skipping uniffi trait `{label}`: {e}");
                None
            }
        }
    };
    if let Some(method) = build(&tm.display_fmt, "Display") {
        traits.push(NitroUniffiTrait::Display { method });
    }
    if let Some(method) = build(&tm.debug_fmt, "Debug") {
        traits.push(NitroUniffiTrait::Debug { method });
    }
    // uniffi also exposes `eq_ne`, but (like the JSI oracle) we render only the
    // `eq_eq` method — `equals()`'s negation is the caller's concern.
    if let Some(method) = build(&tm.eq_eq, "Eq") {
        traits.push(NitroUniffiTrait::Eq { method });
    }
    if let Some(method) = build(&tm.hash_hash, "Hash") {
        traits.push(NitroUniffiTrait::Hash { method });
    }
    if let Some(method) = build(&tm.ord_cmp, "Ord") {
        traits.push(NitroUniffiTrait::Ord { method });
    }
    traits
}

impl NitroFunction {
    /// Author docstring formatted as a JSDoc `/** … */` block for the consumer
    /// surface, or `None` when the function carries no docstring. See
    /// [`format_ts_docstring`] / audit bug #23.
    pub fn ds(&self) -> Option<String> {
        format_ts_docstring(self.docstring.as_deref())
    }

    /// The C++ return type as it appears in the HybridObject method
    /// signature. For sync methods this is the lowered/lifted scalar
    /// type; for async methods it's `std::shared_ptr<Promise<T>>` —
    /// Nitro's contract for `Promise`-returning HybridObject methods.
    pub fn cxx_return_signature(&self) -> String {
        let inner = self.return_kind.cxx_type();
        if self.is_async {
            format!("std::shared_ptr<::margelo::nitro::Promise<{}>>", inner)
        } else {
            inner
        }
    }

    /// TS return type. Async wraps in `Promise<T>` (sync void stays
    /// `void`); throws does not change the type — JS catches via
    /// `try/catch`, so the *shape* of the returned promise is unchanged
    /// whether the underlying call throws or not.
    pub fn ts_return_signature(&self) -> String {
        let inner = self.return_kind.ts_type();
        if self.is_async {
            format!("Promise<{}>", inner)
        } else {
            inner
        }
    }

    /// Per-type C++ headers (`<Record>.hpp` / `<Enum>.hpp` /
    /// `Hybrid<Interface>.hpp`) this function's signature references,
    /// across all args + the return type. Used by the `.hpp` emitters so
    /// the method declarations see the complete struct/enum types.
    fn referenced_headers(&self) -> Vec<String> {
        let mut out = Vec::new();
        for arg in &self.args {
            out.extend(arg.ty.referenced_headers());
        }
        if let ReturnKind::Value(t) = &self.return_kind {
            out.extend(t.referenced_headers());
        }
        out
    }

    /// Record / enum headers this function's args + return need as complete
    /// types (interfaces excluded — they're forward-declared, see
    /// [`Self::interface_classes`]).
    fn value_headers(&self) -> Vec<String> {
        let mut out = Vec::new();
        for arg in &self.args {
            out.extend(arg.ty.referenced_value_headers());
        }
        if let ReturnKind::Value(t) = &self.return_kind {
            out.extend(t.referenced_value_headers());
        }
        out
    }

    /// `(namespace, Hybrid<Name>)` for every interface this function's args +
    /// return reference (for forward declaration in headers).
    fn interface_classes(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for arg in &self.args {
            out.extend(arg.ty.referenced_interface_classes());
        }
        if let ReturnKind::Value(t) = &self.return_kind {
            out.extend(t.referenced_interface_classes());
        }
        out
    }

    /// Collect the foreign namespaces whose record / enum codecs this
    /// function's args + return reach into (so the impl file can include
    /// their `<ns>_codecs.hpp`). See [`NitroType::foreign_codec_namespaces`].
    fn collect_foreign_codec_namespaces(&self, current_ns: &str, out: &mut BTreeSet<String>) {
        for arg in &self.args {
            arg.ty.foreign_codec_namespaces(current_ns, out);
        }
        if let ReturnKind::Value(t) = &self.return_kind {
            t.foreign_codec_namespaces(current_ns, out);
        }
    }
}

pub struct NitroInterface {
    pub ts_name: String,
    pub cxx_class: String,
    /// `uniffi_<crate>_fn_free_<obj>` — comes straight from the pipeline.
    pub free_symbol: String,
    /// `uniffi_<crate>_fn_clone_<obj>` — bumps the Rust-side Arc strong
    /// count. Every call site that hands this object's handle to Rust (the
    /// method receiver, and the object passed as an argument) must clone
    /// first, because uniffi *consumes* the handle it's given.
    pub clone_symbol: String,
    /// The argless / sync / infallible "primary" constructor, if the
    /// interface has one. Holds at most one element (the rest are reshaped
    /// into `factories`). It's wired into the C++ default constructor so
    /// `NitroModules.createHybridObject('<Name>')` yields a live Rust
    /// object; see [`Self::primary_constructor`].
    pub constructors: Vec<NitroFunction>,
    /// Non-primary constructors (argument-taking / async / fallible),
    /// reshaped as factory functions returning this interface. They're
    /// emitted as methods on the namespace API HybridObject because Nitro's
    /// argless `createHybridObject` path can only drive the default
    /// constructor.
    pub factories: Vec<NitroFunction>,
    pub methods: Vec<NitroFunction>,
    /// uniffi object trait impls (`#[uniffi::export(Display, Eq, …)]`) reshaped
    /// into emittable C++ overrides / registered methods. Mirrors the JSI
    /// backend's `obj.uniffi_traits` (`ObjectTemplate.ts`): Display→`toString`,
    /// Debug→`toDebugString`, Eq→`equals`, Hash→`hashCode`, Ord→`compareTo`.
    /// See parity item P2.
    pub uniffi_traits: Vec<NitroUniffiTrait>,
    /// Author docstring from the uniffi metadata, if any (audit bug #23).
    pub docstring: Option<String>,
}

/// A uniffi object trait impl, reshaped for the Nitro HybridObject surface.
/// Each carries the underlying trait `Method` as a [`NitroFunction`] so the
/// existing FFI-symbol / arg-lowering / return-lift machinery is reused; the
/// emitter spells the C++ signature by hand (a base-virtual override for
/// `toString`/`equals`, a plain `registerHybridMethod` for the rest).
///
/// `Eq`/`Ord` carry one interface arg (`other: &Self`), lowered via the same
/// `clone_handle()` path a normal interface arg uses — identical to the JSI
/// oracle's `FfiConverterTypeX.lower(other)`.
///
/// (No `derive` here: `NitroFunction` is intentionally not `Clone/Debug/Eq`,
/// and codegen only ever borrows these — it never clones / compares them.)
pub enum NitroUniffiTrait {
    /// `impl Display` → override the base virtual `std::string toString()`.
    Display { method: NitroFunction },
    /// `impl Debug` → `toDebugString(): string` (plain `registerHybridMethod`).
    /// When Display is absent the JSI oracle aliases `toString`→`toDebugString`;
    /// the emitter mirrors that.
    Debug { method: NitroFunction },
    /// `impl Eq` → override the base virtual
    /// `bool equals(const std::shared_ptr<HybridObject>&)`. Only the `eq_eq`
    /// method is rendered (uniffi also emits `ne`, but the JSI oracle drops it).
    Eq { method: NitroFunction },
    /// `impl Hash` → `hashCode(): bigint` (plain `registerHybridMethod`).
    Hash { method: NitroFunction },
    /// `impl Ord` → `compareTo(other): number` (plain `registerHybridMethod`).
    Ord { method: NitroFunction },
}

impl NitroInterface {
    /// Author docstring formatted as JSDoc, or `None`. See [`format_ts_docstring`].
    pub fn ds(&self) -> Option<String> {
        format_ts_docstring(self.docstring.as_deref())
    }

    fn from_general(iface: &general::Interface) -> Result<Self> {
        let ts_name = iface.name.to_upper_camel_case();
        let cxx_class = format!("Hybrid{}", ts_name);

        // The interface's own type — used as the return of every factory.
        // Resolving via `from_type` yields the correct namespace + name, so
        // the factory's `lift_expr` wraps the owned handle in the right
        // fully-qualified `Hybrid<Name>`.
        let self_ty = NitroType::from_type(&iface.self_type.ty)?;

        // One pass over the constructors. Classification follows uniffi's
        // authoritative `CallableKind::Constructor { primary }` flag (set by
        // name: `new` -> primary; see pipeline/general/callable.rs), NOT a
        // home-grown arity/async/throws heuristic.
        //
        // A ctor lands in `constructors` (wired into the C++ default
        // constructor, driven by `createHybridObject('<Name>')`) ONLY when it
        // is BOTH the primary `new` AND a *drivable default* — argless, sync,
        // infallible — because that's the only shape Nitro's argless
        // `createHybridObject` path can run. Every other constructor (the
        // arg-taking / async / fallible primary, plus all alternates) becomes
        // a factory method on the namespace API; the consumer-module class
        // surface then routes each factory via its `is_primary_ctor` /
        // `static_name` fields (set in `from_constructor_factory`).
        let mut constructors = Vec::new();
        let mut factories = Vec::new();
        for ctor in &iface.constructors {
            let parsed = match NitroFunction::from_constructor(ctor) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!(
                        "nitro: skipping constructor `{}.{}`: {e}",
                        iface.name, ctor.name
                    );
                    continue;
                }
            };
            let is_primary = matches!(
                ctor.callable.kind,
                general::CallableKind::Constructor { primary: true, .. }
            );
            // Whether this ctor can be wired into the C++ argless
            // `createHybridObject` default-ctor path — a real constraint
            // (the default constructor takes no args and cannot await / throw
            // a typed error through that path).
            let is_drivable_default =
                parsed.args.is_empty() && !parsed.is_async && parsed.throws.is_none();
            if is_primary && is_drivable_default {
                constructors.push(parsed);
            } else {
                // Re-derive as a factory (return type = the interface). The
                // plain parse above already succeeded, so this one will too.
                match NitroFunction::from_constructor_factory(ctor, &ts_name, self_ty.clone()) {
                    Ok(f) => factories.push(f),
                    Err(e) => eprintln!(
                        "nitro: skipping constructor factory `{}.{}`: {e}",
                        iface.name, ctor.name
                    ),
                }
            }
        }

        let mut methods = Vec::new();
        for method in &iface.methods {
            match NitroFunction::from_method(method) {
                Ok(f) => methods.push(f),
                Err(e) => eprintln!(
                    "nitro: skipping method `{}.{}`: {e}",
                    iface.name, method.name
                ),
            }
        }

        let uniffi_traits = collect_uniffi_traits(&iface.uniffi_trait_methods);

        Ok(Self {
            ts_name,
            cxx_class,
            free_symbol: iface.ffi_func_free.0.clone(),
            clone_symbol: iface.ffi_func_clone.0.clone(),
            constructors,
            factories,
            methods,
            uniffi_traits,
            docstring: iface.docstring.clone(),
        })
    }

    /// The factory for the uniffi-primary `new` constructor *when it landed in
    /// `factories`* — i.e. the primary takes args / is async / is fallible, so
    /// it could NOT be wired into the C++ argless default-ctor path (which is
    /// `primary_constructor` instead). The consumer-module class template uses
    /// this to emit the JS `constructor(args)` (sync arg-taking / fallible-only)
    /// or a static promise-returning factory (async — JS forbids async
    /// constructors). `None` when the primary is a drivable default (then
    /// `primary_constructor` is `Some`) or when the interface has no primary
    /// (UDL alternate-only objects are rare but valid).
    ///
    /// At most one factory carries `is_primary_ctor` (uniffi marks exactly one
    /// constructor primary), so `find` is unambiguous.
    pub fn primary_ctor_factory(&self) -> Option<&NitroFunction> {
        self.factories.iter().find(|f| f.is_primary_ctor)
    }

    /// The constructor factories the consumer-module class exposes as *named
    /// statics* (`static <ctorName>(...)`) — every alternate / named ctor,
    /// excluding the primary (which is `primary_constructor` /
    /// `primary_ctor_factory`). Each carries a `Some(static_name)`. Iterated by
    /// the TS class template; the bodies delegate to the same `<Ns>Api`
    /// HybridObject factory method (`create<Iface><Ctor>`) the C++ side exposes.
    pub fn static_factories(&self) -> Vec<&NitroFunction> {
        self.factories
            .iter()
            .filter(|f| f.static_name.is_some())
            .collect()
    }

    /// The interface's primary constructor *iff* it is a drivable default
    /// (the uniffi-primary `new` AND argless / sync / infallible), if any.
    /// Nitro vends HybridObjects through
    /// `NitroModules.createHybridObject('<Name>')`, which runs the C++
    /// default constructor with no arguments — so a uniffi constructor can be
    /// wired into that path only when it is the primary and takes no args /
    /// doesn't await / doesn't throw. The common
    /// `#[uniffi::constructor] fn new() -> Arc<Self>` shape fits.
    ///
    /// `from_general` already routes exactly the primary-and-drivable-default
    /// constructor (at most one) into `constructors` and every other ctor into
    /// `factories`, so this is simply the head of `constructors`. Interfaces
    /// whose primary construction needs arguments / is async / is fallible
    /// have an empty `constructors` and surface that ctor as a factory instead.
    pub fn primary_constructor(&self) -> Option<&NitroFunction> {
        self.constructors.first()
    }

    /// `true` when this interface has at least one async *method* — i.e. a
    /// `Promise`-returning HybridObject method whose driver supports
    /// cancellation. Gates emission of the non-spec `__uniffiBeginAbortable()` /
    /// `__uniffiAbort()` cancel hooks on the interface HybridObject (async
    /// constructors land in `factories` on the namespace API, so they're covered
    /// by the namespace-API hooks, not this one).
    pub fn has_async_methods(&self) -> bool {
        self.methods.iter().any(|m| m.is_async)
    }

    /// `true` when this interface has a uniffi `Display` impl. When a `Debug`
    /// impl is present WITHOUT a `Display`, the emitter aliases the base
    /// `toString()` virtual to `toDebugString()` (mirroring the JSI oracle).
    pub fn has_display_trait(&self) -> bool {
        self.uniffi_traits
            .iter()
            .any(|t| matches!(t, NitroUniffiTrait::Display { .. }))
    }

    /// Every uniffi-trait method as a [`NitroFunction`], for the `extern "C"`
    /// FFI-symbol declaration block (each trait method calls a distinct
    /// `…_uniffi_trait_<name>` symbol on the handle(s)).
    pub fn uniffi_trait_functions(&self) -> Vec<&NitroFunction> {
        self.uniffi_traits
            .iter()
            .map(|t| match t {
                NitroUniffiTrait::Display { method }
                | NitroUniffiTrait::Debug { method }
                | NitroUniffiTrait::Hash { method }
                | NitroUniffiTrait::Eq { method }
                | NitroUniffiTrait::Ord { method } => method,
            })
            .collect()
    }

    /// Record / enum headers the `.hpp` must `#include` for complete types in
    /// method signatures (interfaces are forward-declared instead — see
    /// [`Self::interface_forward_decls`] — so they never force a circular
    /// include between two interfaces that reference each other).
    pub fn value_dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.cxx_class);
        dedup_headers(self.methods.iter().flat_map(|m| m.value_headers()), &own)
    }

    /// Interface `Hybrid<Name>.hpp` headers the `.cpp` must `#include` for the
    /// complete type (it constructs `make_shared<Hybrid<Name>>` and calls
    /// methods). Excludes self.
    pub fn interface_dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.cxx_class);
        dedup_headers(
            self.methods.iter().flat_map(|m| {
                m.interface_classes()
                    .into_iter()
                    .map(|(_, cls)| format!("{cls}.hpp"))
            }),
            &own,
        )
    }

    /// Forward declarations (`namespace … { class Hybrid<Name>; }`) the `.hpp`
    /// emits for interface types appearing in method signatures, deduped and
    /// excluding self. Behind a `shared_ptr` a forward declaration is enough,
    /// and it sidesteps circular includes for mutually-referential interfaces.
    pub fn interface_forward_decls(&self) -> Vec<InterfaceFwdDecl> {
        let own = self.cxx_class.clone();
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for m in &self.methods {
            for (namespace, cxx_class) in m.interface_classes() {
                if cxx_class == own {
                    continue;
                }
                if seen.insert((namespace.clone(), cxx_class.clone())) {
                    out.push(InterfaceFwdDecl {
                        namespace,
                        cxx_class,
                    });
                }
            }
        }
        out
    }
}

/// A forward declaration the interface `.hpp` emits for an interface type it
/// references behind a `shared_ptr` (`namespace margelo::nitro::<namespace> {
/// class <cxx_class>; }`).
pub struct InterfaceFwdDecl {
    pub namespace: String,
    pub cxx_class: String,
}

/// One `import type { … } from '<module_path>'` line the `.nitro.ts` spec
/// emits for the cross-namespace types it references. See
/// [`NitroModule::foreign_ts_type_imports`].
pub struct ForeignTsImport {
    /// Relative module path of the foreign namespace's spec, e.g.
    /// `./CelestraShared.nitro`.
    pub module_path: String,
    /// Sorted, deduped TS type names imported from that module.
    pub type_names: Vec<String>,
}

/// One VALUE `import { … } from '<module_path>'` line the consumer wrapper
/// emits for the runtime symbols a configured custom-type conversion needs in
/// scope (e.g. `import { URL } from '@/converters'`). Distinct from
/// [`ForeignTsImport`] (a type-only import); these symbols are referenced by
/// the `intoCustom` / `fromCustom` expressions at runtime. See
/// [`NitroModule::custom_conversion_imports`] / audit bug #17.
pub struct TsNamedImport {
    /// Module specifier the symbols are imported from, verbatim from the
    /// configured `imports = [["URL", "@/converters"]]`.
    pub module_path: String,
    /// Sorted, deduped symbol names imported from that module.
    pub type_names: Vec<String>,
}

/// A foreign-implementable callback interface. Covers two uniffi shapes:
///
/// * UDL `callback interface` / `#[uniffi::export(callback_interface)]`
///   (`general::CallbackInterface`): foreign-ONLY. uniffi emits a vtable
///   `init_fn` but no Rust-callable `fn_method_*` / clone / free symbols.
///   `proxy` is `None`.
///
/// * `#[uniffi::export(with_foreign)]` trait (`general::Interface` with
///   `imp == CallbackTrait`): foreign-implementable AND Rust-callable.
///   uniffi emits the vtable `init_fn` AND per-method `fn_method_*`
///   symbols + `fn_clone_*` / `fn_free_*`, so an `Arc<dyn Trait>` Rust
///   hands back can be wrapped in a proxy `Hybrid<Name>` that dispatches
///   each call back into Rust. `proxy` is `Some`.
///
/// In both cases the JS side implements the methods via a `Hybrid<Name>`
/// subclass; the generated trampoline file (`Hybrid<Name>.{hpp,cpp}`)
/// registers a vtable with Rust on first hand-off so Rust can dispatch
/// into the JS impl. For the `with_foreign` case the SAME class doubles as
/// the Rust-backed proxy: a handle-bearing instance whose (non-overridden)
/// methods call the `fn_method_*` symbols.
pub struct NitroCallbackInterface {
    /// TS spec name, e.g. `ForeignGetters`.
    pub ts_name: String,
    /// C++ class name. Mirrors the interface convention so Nitrogen's
    /// generated spec class is `Hybrid<TsName>Spec`.
    pub cxx_class: String,
    /// Rust symbol that registers a vtable from the foreign side.
    pub vtable_init_symbol: String,
    /// Vtable methods, in declared order. The Rust-side struct is
    /// `(method_ptr, method_ptr, ..., clone_ptr, free_ptr)` — we emit
    /// trampolines for the methods + the clone/free entries.
    pub methods: Vec<NitroCallbackMethod>,
    /// `Some` for `with_foreign` traits: the Rust-callable surface that
    /// lets a returned `Arc<dyn Trait>` be wrapped in a proxy and lets the
    /// proxy clone/free its handle. `None` for foreign-only UDL callback
    /// interfaces.
    pub proxy: Option<NitroCallbackProxy>,
    /// Author docstring from the uniffi metadata, if any (audit bug #23).
    pub docstring: Option<String>,
}

/// Rust-callable surface of a `with_foreign` trait, used to build the
/// Rust-backed proxy half of the generated `Hybrid<Name>` class.
pub struct NitroCallbackProxy {
    /// `uniffi_<crate>_fn_clone_<trait>` — bumps the Rust-side strong
    /// count. The proxy clones before each dispatch (uniffi consumes the
    /// receiver handle), and once when re-vending the handle to Rust.
    pub clone_symbol: String,
    /// `uniffi_<crate>_fn_free_<trait>` — drops the Rust-side reference.
    /// Wired as the proxy handle's RAII free symbol.
    pub free_symbol: String,
}

pub struct NitroCallbackMethod {
    pub ts_name: String,
    pub cxx_name: String,
    pub args: Vec<NitroArg>,
    pub return_kind: ReturnKind,
    /// `true` when the foreign-trait method is an `async fn`. An async
    /// foreign-trait method must surface in TS as `Promise<T>` so the JS
    /// implementation can be `async` (the Nitro runtime awaits the
    /// returned promise before handing the value back to Rust). Sync
    /// methods stay the bare value type. Mirrors `NitroFunction::is_async`.
    pub is_async: bool,
    /// `Some` for `with_foreign` traits' SYNC methods: the Rust-callable
    /// `uniffi_<crate>_fn_method_<trait>_<method>` symbol the proxy
    /// dispatches through. `None` for foreign-only callback interfaces
    /// (no Rust impl exists to call) and for async methods (whose FFI
    /// symbol returns a future handle, not the value — the proxy can't
    /// drive that poll loop yet, so it falls back to the JS-impl path).
    pub uniffi_symbol: Option<String>,
    /// Typed error this method may throw, if any. Drives the proxy's
    /// `lift_<Name>Error` decode + rethrow on a `RustCallStatus` error.
    pub throws: Option<NitroErrorRef>,
    /// Author docstring from the uniffi metadata, if any (audit bug #23).
    pub docstring: Option<String>,
}

impl NitroCallbackMethod {
    /// TS return type for the callback method. An async foreign-trait
    /// method wraps in `Promise<T>` (a sync `void` stays `void`); a sync
    /// method is the bare value type. Mirror of
    /// [`NitroFunction::ts_return_signature`] — a callback method's TS
    /// surface must follow the same async-wrapping rule as an interface
    /// method, because both are foreign-trait methods whose JS impl
    /// returns a value the runtime awaits.
    pub fn ts_return_signature(&self) -> String {
        let inner = self.return_kind.ts_type();
        if self.is_async {
            format!("Promise<{inner}>")
        } else {
            inner
        }
    }

    /// The C++ return type as it appears in the `Hybrid<Name>` method
    /// signature (both the `.hpp` virtual declaration and the `.cpp`
    /// definition). A sync method is the bare lowered/lifted value type; an
    /// async method returns `std::shared_ptr<ForeignAsyncResult<T>>`.
    ///
    /// The wrapper (not a bare `std::shared_ptr<Promise<T>>`) is load-bearing:
    /// the `setJsImpl` hook binds the JS method as a `std::function` of this
    /// same return type, and Nitro's `JSIConverter<std::function<R(Args...)>>`
    /// branches on `is_promise_v<R>`. A `Promise`-typed `R` would build an
    /// `AsyncJSCallback` whose `SyncJSCallback` reads the JS function's
    /// returned *Promise object* as the raw value `T` without awaiting it
    /// (`react-native-nitro-modules` `JSIConverter+Function.hpp` ->
    /// `JSCallback.hpp`). `ForeignAsyncResult<T>` is not a `Promise`, so Nitro
    /// keeps a plain `SyncJSCallback`; its `JSIConverter` (see
    /// `nitro-uniffi/js_async_callback.hpp`) chains `.then` / `.catch` on the
    /// JS Promise and yields a C++ `Promise<T>` the async trampoline awaits
    /// before driving uniffi's foreign-future callback. Unlike
    /// [`NitroFunction::cxx_return_signature`] (an interface/namespace async
    /// method, which returns a real `Promise<T>` *to* JS), a callback method's
    /// async return travels *from* JS and must be awaited.
    pub fn cxx_return_signature(&self) -> String {
        let inner = self.return_kind.cxx_type();
        if self.is_async {
            format!("std::shared_ptr<::ubrn::nitro::ForeignAsyncResult<{inner}>>")
        } else {
            inner
        }
    }

    /// Whether the generated JS-impl bridge (the `setJsImpl` hook + the
    /// per-method `_fn_` member the virtual prefers) can be emitted for this
    /// method. EVERY method is supported.
    ///
    /// The previously-excluded async-void case is now bound exactly like
    /// async-value: the JS-impl member is typed
    /// `std::shared_ptr<ForeignAsyncResult<void>>` (see
    /// [`Self::cxx_return_signature`]), NOT a bare `Promise<void>`. Because
    /// `ForeignAsyncResult<T>` is not a `Promise`, Nitro's
    /// `JSIConverter<std::function<R(Args...)>>` keeps the `SyncJSCallback`
    /// path (its `is_promise_v<R>` test is false), and
    /// `JSIConverter+Promise.hpp` handles the `is_void_v` payload explicitly —
    /// so the JS method's returned Promise is `.then`/`.catch`-chained and
    /// awaited rather than mis-read as the raw value or dropped. Keeping this
    /// `true` for every method makes the consumer module's `impl:` parameter
    /// type (which iterates ALL methods) agree with the `setJsImpl` binding
    /// (which iterates only the supported ones), so async-void trait methods
    /// like `delay`/`tryDelay` are actually bound rather than throwing
    /// "not implemented" at runtime. See audit bug #4.
    pub fn supports_js_impl(&self) -> bool {
        true
    }

    /// Author docstring formatted as JSDoc, or `None`. See [`format_ts_docstring`].
    pub fn ds(&self) -> Option<String> {
        format_ts_docstring(self.docstring.as_deref())
    }
}

impl NitroCallbackInterface {
    /// Author docstring formatted as JSDoc, or `None`. See [`format_ts_docstring`].
    pub fn ds(&self) -> Option<String> {
        format_ts_docstring(self.docstring.as_deref())
    }

    /// Whether any method supports the JS-impl bridge (see
    /// [`NitroCallbackMethod::supports_js_impl`]). When false the `setJsImpl`
    /// hook + the consumer-facing JS-impl factory are not emitted at all.
    pub fn has_js_impl_methods(&self) -> bool {
        self.methods.iter().any(NitroCallbackMethod::supports_js_impl)
    }

    /// The subset of methods the JS-impl bridge is emitted for, in
    /// declaration order. Templates iterate this (rather than filtering
    /// `methods` inline) so `loop.last` drives correct comma separation in
    /// the generated `setJsImpl` parameter list / factory call.
    pub fn js_impl_methods(&self) -> Vec<&NitroCallbackMethod> {
        self.methods
            .iter()
            .filter(|m| m.supports_js_impl())
            .collect()
    }

    /// Build the foreign-only shape from a UDL `callback interface`.
    fn from_general(cb: &general::CallbackInterface) -> Result<Self> {
        let ts_name = cb.name.to_upper_camel_case();
        let cxx_class = format!("Hybrid{}", ts_name);
        let vtable_init_symbol = cb.vtable.init_fn.0.clone();

        let mut methods = Vec::new();
        for method in &cb.methods {
            let res = (|| -> Result<NitroCallbackMethod> {
                let ts_name = ts_fn_name(&method.name);
                let cxx_name = sanitize_cxx_ident(&method.name.to_lower_camel_case());
                let mut args = Vec::new();
                for arg in &method.inputs {
                    args.push(NitroArg::from_general(arg)?);
                }
                let return_kind = ReturnKind::from_general(method.return_type.as_ref())?;
                Ok(NitroCallbackMethod {
                    ts_name,
                    cxx_name,
                    args,
                    return_kind,
                    is_async: method.is_async,
                    // A UDL callback interface has no Rust impl, so there is
                    // no `fn_method_*` symbol and no proxy dispatch.
                    uniffi_symbol: None,
                    throws: throws_from(method.throws.as_ref()),
                    docstring: method.docstring.clone(),
                })
            })();
            match res {
                Ok(m) => methods.push(m),
                Err(e) => eprintln!(
                    "nitro: skipping callback method `{}.{}`: {e}",
                    cb.name, method.name
                ),
            }
        }
        Ok(Self {
            ts_name,
            cxx_class,
            vtable_init_symbol,
            methods,
            proxy: None,
            docstring: cb.docstring.clone(),
        })
    }

    /// Build the dual (foreign-implementable + Rust-callable proxy) shape
    /// from a `#[uniffi::export(with_foreign)]` trait, which the pipeline
    /// represents as an `Interface` with `imp == CallbackTrait` and a
    /// `vtable`.
    fn from_foreign_trait(iface: &general::Interface) -> Result<Self> {
        let ts_name = iface.name.to_upper_camel_case();
        let cxx_class = format!("Hybrid{}", ts_name);
        // A `CallbackTrait` Interface always carries a vtable (that's what
        // makes it foreign-implementable); fail loudly rather than guessing
        // the init symbol if uniffi's invariant ever changes.
        let vtable = iface.vtable.as_ref().ok_or_else(|| {
            anyhow!(
                "nitro: with_foreign trait `{}` has no vtable in uniffi metadata",
                iface.name
            )
        })?;
        let vtable_init_symbol = vtable.init_fn.0.clone();

        let mut methods = Vec::new();
        for method in &iface.methods {
            let res = (|| -> Result<NitroCallbackMethod> {
                let ts_name = ts_fn_name(&method.name);
                let cxx_name = sanitize_cxx_ident(&method.name.to_lower_camel_case());
                let mut args = Vec::new();
                for arg in &method.inputs {
                    args.push(NitroArg::from_general(arg)?);
                }
                let return_kind = ReturnKind::from_general(method.return_type.as_ref())?;
                // Only sync methods get a proxy dispatch symbol. An async
                // trait method's `fn_method_*` returns a future handle, not
                // the value; driving that poll loop from inside a synchronous
                // HybridObject method body isn't possible, so we leave the
                // proxy half unimplemented (it falls back to the JS-impl
                // throw) and route async callbacks through the foreign-impl
                // path only — matching the rest of the callback template,
                // which is sync-only.
                let uniffi_symbol = if method.is_async {
                    None
                } else {
                    Some(method.callable.ffi_func.0.clone())
                };
                Ok(NitroCallbackMethod {
                    ts_name,
                    cxx_name,
                    args,
                    return_kind,
                    is_async: method.is_async,
                    uniffi_symbol,
                    throws: throws_from(method.throws.as_ref()),
                    docstring: method.docstring.clone(),
                })
            })();
            match res {
                Ok(m) => methods.push(m),
                Err(e) => eprintln!(
                    "nitro: skipping callback method `{}.{}`: {e}",
                    iface.name, method.name
                ),
            }
        }
        Ok(Self {
            ts_name,
            cxx_class,
            vtable_init_symbol,
            methods,
            proxy: Some(NitroCallbackProxy {
                clone_symbol: iface.ffi_func_clone.0.clone(),
                free_symbol: iface.ffi_func_free.0.clone(),
            }),
            docstring: iface.docstring.clone(),
        })
    }

    /// Record / enum headers the callback `.hpp` needs as *complete* types in
    /// method signatures (interfaces are forward-declared instead).
    pub fn value_dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.cxx_class);
        let headers = self.methods.iter().flat_map(|m| {
            let mut out = Vec::new();
            for arg in &m.args {
                out.extend(arg.ty.referenced_value_headers());
            }
            if let ReturnKind::Value(t) = &m.return_kind {
                out.extend(t.referenced_value_headers());
            }
            out
        });
        dedup_headers(headers, &own)
    }

    /// Interface `Hybrid<Name>.hpp` headers the callback `.cpp` includes for
    /// complete types (the proxy / trampoline constructs + calls them).
    pub fn interface_dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.cxx_class);
        let headers = self.methods.iter().flat_map(|m| {
            let mut out: Vec<String> = Vec::new();
            for arg in &m.args {
                out.extend(
                    arg.ty
                        .referenced_interface_classes()
                        .into_iter()
                        .map(|(_, c)| format!("{c}.hpp")),
                );
            }
            if let ReturnKind::Value(t) = &m.return_kind {
                out.extend(
                    t.referenced_interface_classes()
                        .into_iter()
                        .map(|(_, c)| format!("{c}.hpp")),
                );
            }
            out
        });
        dedup_headers(headers, &own)
    }

    /// Forward declarations for interface types in callback method signatures
    /// (behind `shared_ptr`, so a declaration suffices; avoids circular
    /// includes). Excludes self.
    pub fn interface_forward_decls(&self) -> Vec<InterfaceFwdDecl> {
        let own = self.cxx_class.clone();
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for m in &self.methods {
            let mut tys: Vec<&NitroType> = m.args.iter().map(|a| &a.ty).collect();
            if let ReturnKind::Value(t) = &m.return_kind {
                tys.push(t);
            }
            for ty in tys {
                for (namespace, cxx_class) in ty.referenced_interface_classes() {
                    if cxx_class == own {
                        continue;
                    }
                    if seen.insert((namespace.clone(), cxx_class.clone())) {
                        out.push(InterfaceFwdDecl {
                            namespace,
                            cxx_class,
                        });
                    }
                }
            }
        }
        out
    }
}

pub struct NitroArg {
    pub ts_name: String,
    pub ty: NitroType,
    /// The UDL/proc-macro-declared default for this argument, rendered as a TS
    /// literal, if any. Emitted as `= <dv>` ONLY in the CONSUMER `namespace.ts`
    /// implementation arg lists (top-level fns, interface constructor, static
    /// factories) — never in the type-only `.nitro.ts` spec interfaces or the
    /// `as unknown as { … }` call-signature type literal (both reject `= dv`).
    /// Mirrors gen_typescript's `build_arg` (`api_module/builders.rs`). See
    /// parity item P3.
    pub default_value: Option<String>,
}

impl NitroArg {
    fn from_general(arg: &general::Argument) -> Result<Self> {
        Ok(Self {
            ts_name: sanitize_ts_arg_ident(&arg.name.to_lower_camel_case()),
            ty: NitroType::from_type(&arg.ty.ty)?,
            default_value: arg.default.as_ref().map(render_default_value),
        })
    }

    /// Convenience for callback trampoline templates: lift the arg
    /// from its `<ts_name>_lowered` (C-ABI) form to the C++ value.
    /// Used inside the per-callback-method `extern "C"` trampoline,
    /// which receives the Rust-lowered shape and needs to call into
    /// the foreign HybridObject method with C++ types. `current_ns` is the
    /// namespace whose codecs header the trampoline includes, so a record /
    /// enum from another namespace gets foreign-qualified.
    pub fn lifted_from_lowered_expr(&self, current_ns: &str) -> String {
        let lowered_name = format!("{}_lowered", self.ts_name);
        self.ty.lift_expr(&lowered_name, current_ns)
    }

    /// The exception-safe argument-lowering *declaration* statement for an
    /// outbound FFI call (interface method, namespace-API free function, or
    /// callback proxy dispatch). Implements the three-way split (audit bug
    /// #24): an owning `RustBuffer` arg is parked in a `RustBufferGuard`; an
    /// interface / callback handle arg is parked in a move-only
    /// `UniffiObjectHandle<&free_symbol>` guard; everything else stays a bare
    /// `auto <name>_lowered = …` local. Each guarded value is `.take()`-d into
    /// the FFI call by [`Self::pass_expr`] only once *every* arg has lowered
    /// successfully, so a later throwing lowering frees the already-built
    /// buffers / cloned handles on unwind instead of leaking them.
    ///
    /// A handle-bearing arg whose `free_symbol` is unknown (an unregistered
    /// type, e.g. a direct unit-test `NitroType`) falls back to a bare local —
    /// it cannot be guarded without the symbol, and that path is not reached by
    /// real generated code (every emitted interface registers its free symbol).
    pub fn lower_guard_stmt(
        &self,
        current_ns: &str,
        alloc_symbol: &str,
        reserve_symbol: &str,
    ) -> String {
        let lowered = self
            .ty
            .lower_expr(&self.ts_name, current_ns, alloc_symbol, reserve_symbol);
        if self.ty.is_rust_buffer() {
            format!(
                "ubrn::nitro::RustBufferGuard {name}_guard{{ {lowered}, &free_status_buffer }};",
                name = self.ts_name,
            )
        } else if let Some(free) = self.handle_guard_free_symbol() {
            format!(
                "ubrn::nitro::UniffiObjectHandle<&{free}> {name}_guard{{ {lowered} }};",
                name = self.ts_name,
            )
        } else {
            format!("auto {name}_lowered = {lowered};", name = self.ts_name)
        }
    }

    /// The call-site expression for this arg in an outbound FFI call: the
    /// guarded forms relinquish ownership via `.take()`, the bare form passes
    /// its `<name>_lowered` local directly. Mirrors [`Self::lower_guard_stmt`].
    pub fn pass_expr(&self) -> String {
        if self.ty.is_rust_buffer() || self.handle_guard_free_symbol().is_some() {
            format!("{}_guard.take()", self.ts_name)
        } else {
            format!("{}_lowered", self.ts_name)
        }
    }

    /// `Some(free_symbol)` when this arg is a handle-bearing type whose owning
    /// `fn_free_<obj>` symbol is known — i.e. it both needs a move-only handle
    /// guard AND we can name the free hook to instantiate one. Used by both
    /// helpers above so the declaration and the call site agree on whether the
    /// arg is guarded.
    fn handle_guard_free_symbol(&self) -> Option<String> {
        if self.ty.needs_handle_guard() {
            self.ty.free_symbol()
        } else {
            None
        }
    }
}

/// The subset of uniffi types ubrn's Nitro backend currently emits.
/// Primitives + bool + string flow through `converters.hpp`; composite
/// types (Option / Vec / HashMap / Vec<u8>) flow through
/// `composites.hpp` and are recursively composed via function-pointer
/// template parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NitroType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    String,
    Bytes,
    /// `std::time::SystemTime`. Wire format: `RustBuffer` containing
    /// `i64 seconds + u32 nanos` (12 bytes, big-endian) where `seconds`
    /// is the signed offset from the Unix epoch and `nanos` is the
    /// non-negative subsecond component. TS surface: `Date`. C++
    /// surface: `std::chrono::system_clock::time_point`.
    Timestamp,
    /// `std::time::Duration`. Wire format: `RustBuffer` containing
    /// `u64 seconds + u32 nanos` (12 bytes, big-endian). TS surface:
    /// `number` (milliseconds, matching the TS/JSI backend). C++
    /// surface: `double` (milliseconds) — see `cxx_type`. This is a
    /// deliberate, JS-friendly surface that matches the JSI backend; it
    /// loses exactness for sub-millisecond nanos and durations beyond
    /// 2^53 ms (~285k millennia) vs uniffi's exact u64+u32 representation.
    Duration,
    Optional(Box<NitroType>),
    Sequence(Box<NitroType>),
    Map(Box<NitroType>, Box<NitroType>),
    /// A foreign-implemented callback. The C++ side accepts a
    /// `std::shared_ptr<Hybrid<Name>>` and registers it with the
    /// vtable handle map before passing the handle to Rust.
    ///
    /// `namespace` is the owning namespace of the callback's generated
    /// `Hybrid<Name>` class / `ensure_<Name>_vtable_init` hook (both live in
    /// `margelo::nitro::<namespace>`). A cross-namespace callback (a
    /// `with_foreign` trait exported by another crate, used inside this
    /// module) must be spelled fully-qualified; a same-namespace callback
    /// stays unqualified. An empty `namespace` means "same namespace /
    /// unqualified" — see [`Self::codec_ns_prefix`] / the consumers.
    CallbackInterface {
        namespace: String,
        name: String,
    },
    /// A uniffi `dictionary` / Rust struct. Crosses the FFI as a
    /// `RustBuffer`; lift/lower expressions delegate to the per-record
    /// codec emitted in `<namespace>_codecs.hpp`.
    Record {
        namespace: String,
        name: String,
    },
    /// A uniffi enum. `flat=true` means every variant has zero fields —
    /// the enum is encoded as an `i32` ordinal. Tagged enums use the
    /// same RustBuffer transport but reserve future codec extensions.
    Enum {
        namespace: String,
        name: String,
    },
    /// A uniffi `interface` (Rust struct). Crosses the FFI as a `uint64_t`
    /// handle. Lowering reads the handle out of a `std::shared_ptr<Hybrid<Name>>`
    /// via the impl's public `raw_handle()` accessor; lifting wraps the raw
    /// handle in `std::make_shared<Hybrid<Name>>(raw)`.
    Interface {
        namespace: String,
        name: String,
    },
    /// A uniffi `custom` type (newtype) — a thin nominal wrapper around a
    /// `builtin` (e.g. `Url`/`String`, `Handle`/`Int64`). The wire format and
    /// every C++ codec / lift / lower / header path are exactly the `inner`
    /// builtin's (uniffi gives a custom type no FFI identity of its own), so
    /// all those methods delegate straight through to `inner`. Only the
    /// TS-surface spelling differs: [`Self::ts_type`] returns the nominal
    /// `name` alias (the consumer module emits `export type <Name> = <inner>`),
    /// and the configured `intoCustom` / `fromCustom` conversions (threaded via
    /// the per-namespace `TsConfig`) are applied in the plain-TS wrapper. See
    /// audit bug #17. `namespace` is the owning uniffi namespace (drives the
    /// cross-namespace `import type` of the alias).
    Custom {
        namespace: String,
        name: String,
        inner: Box<NitroType>,
    },
    /// A placeholder for any uniffi type the Nitro backend can't model.
    /// `from_type` is total over uniffi's type universe today, so this is
    /// unreachable in practice; it remains as a defensive surface
    /// (`unknown` in TS, runtime-throwing codec thunks) so a future
    /// uniffi type addition degrades loudly rather than failing the whole
    /// module emission.
    Stub,
}

impl NitroType {
    /// The fixed number of wire bytes this type serializes to, if constant.
    /// Returns `None` for variable-width types (string, bytes, optional,
    /// sequence, map, record, enum, object/callback handles). Used purely for
    /// `reserve_additional` capacity hints — never affects the bytes actually
    /// written. Widths match the big-endian primitive encodings in
    /// `nitro-uniffi/rust_buffer.hpp` (`bool` is wire-encoded as `i8`).
    pub fn fixed_wire_width(&self) -> Option<usize> {
        match self {
            Self::U8 | Self::I8 | Self::Bool => Some(1),
            Self::U16 | Self::I16 => Some(2),
            Self::U32 | Self::I32 | Self::F32 => Some(4),
            Self::U64 | Self::I64 | Self::F64 => Some(8),
            Self::Custom { inner, .. } => inner.fixed_wire_width(),
            _ => None,
        }
    }

    pub fn from_type(ty: &general::Type) -> Result<Self> {
        use general::Type;
        Ok(match ty {
            Type::Boolean => Self::Bool,
            Type::UInt8 => Self::U8,
            Type::UInt16 => Self::U16,
            Type::UInt32 => Self::U32,
            Type::UInt64 => Self::U64,
            Type::Int8 => Self::I8,
            Type::Int16 => Self::I16,
            Type::Int32 => Self::I32,
            Type::Int64 => Self::I64,
            Type::Float32 => Self::F32,
            Type::Float64 => Self::F64,
            Type::String => Self::String,
            Type::Bytes => Self::Bytes,
            Type::Timestamp => Self::Timestamp,
            Type::Duration => Self::Duration,
            Type::Optional { inner_type } => Self::Optional(Box::new(Self::from_type(inner_type)?)),
            Type::Sequence { inner_type } => Self::Sequence(Box::new(Self::from_type(inner_type)?)),
            Type::Map {
                key_type,
                value_type,
            } => Self::Map(
                Box::new(Self::from_type(key_type)?),
                Box::new(Self::from_type(value_type)?),
            ),
            Type::CallbackInterface { namespace, name } => Self::CallbackInterface {
                namespace: namespace.clone(),
                name: name.to_upper_camel_case(),
            },
            Type::Record { namespace, name } => Self::Record {
                namespace: namespace.clone(),
                name: name.clone(),
            },
            Type::Enum { namespace, name } => Self::Enum {
                namespace: namespace.clone(),
                name: name.clone(),
            },
            // A `with_foreign` trait — foreign-implementable, so its use site
            // must round-trip through the callback `Hybrid<Name>` proxy, not
            // the plain-interface handle path. uniffi reports `imp ==
            // CallbackTrait` for a proc-macro `#[uniffi::export(with_foreign)]`
            // trait, but a UDL `[Trait, WithForeign]` trait *imported into
            // another crate via `typedef trait`* loses the `WithForeign` bit
            // at the use site (`imp == Trait`). So a plain `imp == Trait` is
            // additionally checked against the cross-namespace registry of
            // foreign-implementable traits (populated from every namespace's
            // *definitions*, which always carry the authoritative `imp`).
            Type::Interface {
                namespace,
                name,
                imp,
            } => {
                let is_callback = matches!(imp, general::ObjectImpl::CallbackTrait)
                    || is_registered_callback_trait(namespace, name);
                if is_callback {
                    Self::CallbackInterface {
                        namespace: namespace.clone(),
                        name: name.to_upper_camel_case(),
                    }
                } else {
                    Self::Interface {
                        namespace: namespace.clone(),
                        name: name.clone(),
                    }
                }
            }
            // A uniffi `custom` type (newtype) is a thin nominal wrapper around
            // a builtin (e.g. `Url`/`String`, `JsonValue`/`String`) whose only
            // role on the FFI side is to inherit the builtin's wire format. We
            // PRESERVE the nominal `name` (so the TS surface keeps the alias and
            // configured converters can apply) while recursing into `builtin`
            // for the wire path — every codec / C++ / header method on
            // `Self::Custom` delegates to `inner`, so the C ABI is unchanged vs
            // the previous erase-to-builtin behavior. See audit bug #17.
            Type::Custom {
                namespace,
                name,
                builtin,
            } => Self::Custom {
                namespace: namespace.clone(),
                name: name.to_upper_camel_case(),
                inner: Box::new(Self::from_type(builtin)?),
            },
        })
    }

    /// Like [`Self::from_type`] but lossy — returns [`Self::Stub`] for any
    /// type we don't yet know how to round-trip. Used inside record /
    /// enum field walks where bailing out of the whole emission for one
    /// unsupported field type would block far too much downstream work.
    pub fn from_type_lossy(ty: &general::Type) -> Self {
        Self::from_type(ty).unwrap_or(Self::Stub)
    }

    /// TS type spelling. Composites recurse; callbacks use the spec
    /// interface name directly.
    pub fn ts_type(&self) -> String {
        match self {
            Self::Bool => "boolean".into(),
            Self::U8 | Self::U16 | Self::U32 | Self::I8 | Self::I16 | Self::I32 => "number".into(),
            Self::F32 | Self::F64 => "number".into(),
            Self::U64 | Self::I64 => "bigint".into(),
            Self::String => "string".into(),
            Self::Bytes => "ArrayBuffer".into(),
            Self::Timestamp => "Date".into(),
            Self::Duration => "number".into(),
            // `| undefined`, not `| null`: at the HybridObject boundary
            // optionals are converted by Nitro core's
            // `JSIConverter<std::optional<T>>`, which maps `nullopt` to/from
            // JS `undefined` only (a literal `null` fails `canConvert`). ubrn
            // ships no override (and couldn't without an ODR clash against
            // Nitro's `final` specialization), so the generated type must
            // spell the only achievable runtime semantics — `T | undefined` —
            // matching how Nitro itself models optionals and the idiomatic
            // `field?:` / `obj?.x` usage.
            Self::Optional(inner) => format!("({}) | undefined", inner.ts_type()),
            // Canonical (`gen_typescript` `type_helpers.rs`) emits `Array<T>`,
            // not `(T)[]`; structurally identical, but we mirror the oracle for
            // parity. See audit bug #14.
            Self::Sequence(inner) => format!("Array<{}>", inner.ts_type()),
            // Always `Map<K, V>` (a real JS `Map`), NEVER `Record<K, V>` (a
            // plain object). A `Record<number, V>` stringifies its numeric keys
            // at the C++ converter boundary, breaking `.get(<numericKey>)` /
            // `.size` / `for…of` on the JS side; the paired C++ map converter
            // marshals a real JS `Map` for any key type. See audit bug #12.
            Self::Map(k, v) => format!("Map<{}, {}>", k.ts_type(), v.ts_type()),
            Self::CallbackInterface { name, .. } => name.clone(),
            Self::Record { name, .. } => name.to_upper_camel_case(),
            Self::Enum { name, .. } => name.to_upper_camel_case(),
            Self::Interface { name, .. } => name.to_upper_camel_case(),
            // The nominal alias — the consumer module declares
            // `export type <Name> = <inner>` so this resolves. See bug #17.
            Self::Custom { name, .. } => name.clone(),
            Self::Stub => "unknown".into(),
        }
    }

    /// Whether a configured custom-type `intoCustom` / `fromCustom` conversion
    /// can be applied at the wrapper boundary when THIS type is the custom's
    /// inner (wire) builtin. True only for scalar builtins that have a plain JS
    /// runtime VALUE form (`string`, `number`, `bigint`, `boolean`, `Date`,
    /// `ArrayBuffer`) — the conversion expressions reference only the value
    /// plus imported helpers, so they resolve. A custom over a Record / Enum /
    /// Interface (or a composite) is Nitro-limited: those surface as TYPE-ONLY
    /// under Nitro (no runtime `new MyEnum.A()` / `MyEnum_Tags`), so a JSI-style
    /// conversion expression that constructs them would not type-check. For
    /// those the wrapper falls back to a plain pass-through alias to the inner
    /// type (the same documented limitation as record-field conversion). #17.
    fn inner_supports_js_conversion(&self) -> bool {
        matches!(
            self,
            Self::Bool
                | Self::U8
                | Self::U16
                | Self::U32
                | Self::U64
                | Self::I8
                | Self::I16
                | Self::I32
                | Self::I64
                | Self::F32
                | Self::F64
                | Self::String
                | Self::Bytes
                | Self::Timestamp
                | Self::Duration
        )
    }

    /// C++ type spelled in the HybridObject method signature.
    pub fn cxx_type(&self) -> String {
        match self {
            Self::Bool => "bool".into(),
            Self::U8 => "uint8_t".into(),
            Self::U16 => "uint16_t".into(),
            Self::U32 => "uint32_t".into(),
            Self::U64 => "uint64_t".into(),
            Self::I8 => "int8_t".into(),
            Self::I16 => "int16_t".into(),
            Self::I32 => "int32_t".into(),
            Self::I64 => "int64_t".into(),
            Self::F32 => "float".into(),
            Self::F64 => "double".into(),
            Self::String => "std::string".into(),
            // `Vec<u8>` ⇄ Nitro `ArrayBuffer` (JS `ArrayBuffer`): one copy
            // per direction instead of two, and zero-copy on the JS read
            // side. See `nitro-uniffi/composites.hpp` bytes section.
            Self::Bytes => "std::shared_ptr<::margelo::nitro::ArrayBuffer>".into(),
            Self::Timestamp => "std::chrono::system_clock::time_point".into(),
            // uniffi-rs Durations are non-negative; the TS/JSI backend
            // exposes them as a JS `number` (milliseconds). Nitrogen
            // converts a TS `number` field to C++ `double`, so we
            // mirror that to keep the codec compatible with Nitrogen's
            // generated struct/method shapes.
            Self::Duration => "double".into(),
            Self::Optional(inner) => format!("std::optional<{}>", inner.cxx_type()),
            Self::Sequence(inner) => format!("std::vector<{}>", inner.cxx_type()),
            Self::Map(k, v) => format!("std::unordered_map<{}, {}>", k.cxx_type(), v.cxx_type()),
            Self::CallbackInterface { namespace, name } => {
                format!(
                    "std::shared_ptr<{prefix}Hybrid{name}>",
                    prefix = Self::cxx_class_ns_prefix(namespace),
                    name = name.to_upper_camel_case(),
                )
            }
            // Records / enums live in `margelo::nitro::<namespace>`. Always
            // fully-qualify so the spelling resolves both inside that
            // namespace (method signatures, codecs) and inside the bare
            // `margelo::nitro` (where the `JSIConverter` specializations and
            // composite element types — `std::vector<Tree>` — are named).
            Self::Record { namespace, name } | Self::Enum { namespace, name } => format!(
                "::margelo::nitro::{}::{}",
                namespace,
                name.to_upper_camel_case()
            ),
            Self::Interface { namespace, name } => format!(
                "std::shared_ptr<::margelo::nitro::{}::Hybrid{}>",
                namespace,
                name.to_upper_camel_case()
            ),
            // A custom type has no C++ identity of its own — it is the inner
            // builtin's C++ type on the wire (see the `Custom` doc-comment).
            Self::Custom { inner, .. } => inner.cxx_type(),
            Self::Stub => "::ubrn::nitro::StubValue".into(),
        }
    }

    /// C type as it appears in the uniffi `extern "C"` declaration.
    /// Strings + composites cross as `RustBuffer`; callback interfaces
    /// as `uint64_t` handles; bool as `int8_t`.
    pub fn c_type(&self) -> &'static str {
        match self {
            Self::Bool => "int8_t",
            Self::U8 => "uint8_t",
            Self::U16 => "uint16_t",
            Self::U32 => "uint32_t",
            Self::U64 => "uint64_t",
            Self::I8 => "int8_t",
            Self::I16 => "int16_t",
            Self::I32 => "int32_t",
            Self::I64 => "int64_t",
            Self::F32 => "float",
            Self::F64 => "double",
            Self::String
            | Self::Bytes
            | Self::Timestamp
            | Self::Duration
            | Self::Optional(_)
            | Self::Sequence(_)
            | Self::Map(_, _)
            | Self::Record { .. }
            | Self::Enum { .. }
            | Self::Stub => "RustBuffer",
            Self::CallbackInterface { .. } | Self::Interface { .. } => "uint64_t",
            // The wire form is the inner builtin's (see the `Custom` doc-comment).
            Self::Custom { inner, .. } => inner.c_type(),
        }
    }

    /// True when this type crosses the FFI as a `RustBuffer`. Used by the
    /// callback trampolines: an arg lowered by Rust into a RustBuffer is
    /// handed to us by-value (Rust gives up ownership), so after lifting it
    /// the trampoline must free the buffer exactly once.
    pub fn is_rust_buffer(&self) -> bool {
        self.c_type() == "RustBuffer"
    }

    /// The owning interface / callback-proxy `uniffi_<crate>_fn_free_<obj>`
    /// symbol for a handle-bearing type (`Interface` or `CallbackInterface`),
    /// recovered from the per-generate [`INTERFACE_FREE_SYMBOLS`] registry by
    /// `(namespace, name)` — the use-site `Type` carries no symbol of its own.
    /// Drives the move-only RAII handle guard (`UniffiObjectHandle<&free>`) that
    /// the arg-lowering / return-handle paths use so a cloned Arc / handle-map
    /// entry isn't leaked on a throwing-lowering or `make_shared` unwind (audit
    /// bugs #24/#27). A `Custom` delegates to its inner; everything else (and an
    /// unregistered type — e.g. a direct unit-test `NitroType`) is `None`, in
    /// which case the caller falls back to the bare-local lowering.
    pub fn free_symbol(&self) -> Option<String> {
        match self {
            Self::Interface { namespace, name } | Self::CallbackInterface { namespace, name } => {
                registered_interface_free_symbol(namespace, name)
            }
            Self::Custom { inner, .. } => inner.free_symbol(),
            _ => None,
        }
    }

    /// Whether an *argument* of this type, once lowered to its `uint64_t`
    /// handle, must be parked in a move-only RAII handle guard (released into
    /// the FFI call only after every arg lowering has succeeded) rather than
    /// held in a bare local — true for `Interface` / `CallbackInterface` args,
    /// whose lowering clones an Arc / inserts a handle-map entry that a later
    /// throwing arg lowering would otherwise leak. See audit bug #24/#25.
    pub fn needs_handle_guard(&self) -> bool {
        match self {
            Self::Interface { .. } | Self::CallbackInterface { .. } => true,
            Self::Custom { inner, .. } => inner.needs_handle_guard(),
            _ => false,
        }
    }

    // NOTE: the owned-handle return guard (audit bug #27) needs no predicate —
    // `lift_expr`'s `Interface` / `CallbackInterface` arms unconditionally route
    // the returned Arc handle through the `Hybrid<Name>::adopt` choke point,
    // which parks it in a move-only guard THROUGH the `make_shared` allocation.
    // So the F2 fix is structural in `lift_expr` and a `returns_owned_handle`
    // predicate would be dead — it is intentionally not carried.

    /// Qualifier prefix for a record / enum codec free-function
    /// (`write_/read_/lower_/lift_<Name>`) defined in `type_ns`, as seen
    /// from a codec / impl file emitted for `current_ns`.
    ///
    /// Same-namespace types stay *unqualified* — the call site sits inside
    /// `namespace margelo::nitro::<current_ns>` (codecs) or includes that
    /// namespace's codecs header (impl files), so unqualified resolves and
    /// keeps the common case terse + churn-free. A foreign type's codec
    /// lives in the *foreign* namespace's `<foreign_ns>_codecs.hpp`, so it
    /// must be fully qualified with `::margelo::nitro::<foreign_ns>::` —
    /// mirroring how [`Self::cxx_type`] / the `Interface` arms always
    /// qualify `Hybrid<Name>` with its owning namespace.
    fn codec_ns_prefix(type_ns: &str, current_ns: &str) -> String {
        if type_ns == current_ns {
            String::new()
        } else {
            format!("::margelo::nitro::{type_ns}::")
        }
    }

    /// Absolute `::margelo::nitro::<ns>::` qualifier for a generated C++
    /// *class* / free function (`Hybrid<Name>`, `ensure_<Name>_vtable_init`)
    /// living in namespace `type_ns`. Unlike [`Self::codec_ns_prefix`] this
    /// is independent of the emitting file's namespace — an absolute path
    /// resolves everywhere, including from inside `margelo::nitro::<type_ns>`
    /// itself — mirroring how the `Interface` arms always fully-qualify
    /// `Hybrid<Name>`. An empty `type_ns` (a same-namespace UDL callback for
    /// which `from_type` saw no foreign namespace) stays unqualified.
    fn cxx_class_ns_prefix(type_ns: &str) -> String {
        if type_ns.is_empty() {
            String::new()
        } else {
            format!("::margelo::nitro::{type_ns}::")
        }
    }

    /// Expression that lowers a `cxx_type`-typed value named `<name>` to
    /// the `c_type` for an FFI call. `current_ns` is the namespace of the
    /// file this expression is emitted into (so a record / enum codec call
    /// to a foreign namespace's type gets fully qualified). `alloc_symbol`
    /// / `reserve_symbol` are the namespace's `ffi_<crate>_rustbuffer_alloc`
    /// / `ffi_<crate>_rustbuffer_reserve` symbols — both needed by the
    /// `RustBufferWriter` template.
    pub fn lower_expr(
        &self,
        name: &str,
        current_ns: &str,
        alloc_symbol: &str,
        reserve_symbol: &str,
    ) -> String {
        match self {
            Self::Bool => format!("ubrn::nitro::lower_bool({name})"),
            Self::String => format!("ubrn::nitro::lower_string<&{alloc_symbol}>({name})"),
            Self::Bytes => {
                format!("ubrn::nitro::lower_bytes<&{alloc_symbol}, &{reserve_symbol}>({name})")
            }
            Self::Timestamp => {
                format!("ubrn::nitro::lower_timestamp<&{alloc_symbol}, &{reserve_symbol}>({name})")
            }
            Self::Duration => {
                format!("ubrn::nitro::lower_duration<&{alloc_symbol}, &{reserve_symbol}>({name})")
            }
            Self::Optional(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_writer =
                    inner.write_fn_template_arg(current_ns, alloc_symbol, reserve_symbol);
                format!(
                    "ubrn::nitro::lower_optional<{ty}, &{alloc}, &{reserve}, {writer}>({name})",
                    ty = inner_cxx,
                    alloc = alloc_symbol,
                    reserve = reserve_symbol,
                    writer = inner_writer,
                )
            }
            Self::Sequence(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_writer =
                    inner.write_fn_template_arg(current_ns, alloc_symbol, reserve_symbol);
                format!(
                    "ubrn::nitro::lower_sequence<{ty}, &{alloc}, &{reserve}, {writer}>({name})",
                    ty = inner_cxx,
                    alloc = alloc_symbol,
                    reserve = reserve_symbol,
                    writer = inner_writer,
                )
            }
            Self::Map(k, v) => {
                let k_cxx = k.cxx_type();
                let v_cxx = v.cxx_type();
                let k_writer = k.write_fn_template_arg(current_ns, alloc_symbol, reserve_symbol);
                let v_writer = v.write_fn_template_arg(current_ns, alloc_symbol, reserve_symbol);
                format!(
                    "ubrn::nitro::lower_map<{kt}, {vt}, &{alloc}, &{reserve}, {kw}, {vw}>({name})",
                    kt = k_cxx,
                    vt = v_cxx,
                    alloc = alloc_symbol,
                    reserve = reserve_symbol,
                    kw = k_writer,
                    vw = v_writer,
                )
            }
            Self::CallbackInterface { namespace, name: cb_name } => {
                // Install the vtable (idempotent) before the first hand-off,
                // then turn the `shared_ptr<Hybrid<Name>>` into the u64 handle
                // Rust names it by: a JS-implemented instance is registered
                // with the handle map; a Rust-backed proxy (e.g. handed back
                // to Rust) clones its existing handle. Both paths live in
                // `Hybrid<Name>::lower_to_handle`. The comma operator keeps
                // this a single expression usable in `auto x = …;`. The class +
                // vtable hook live in the callback's owning namespace, so
                // qualify when it's foreign.
                format!(
                    "({prefix}Hybrid{cb}::ensure_vtable(), {name}->lower_to_handle({name}))",
                    prefix = Self::cxx_class_ns_prefix(namespace),
                    cb = cb_name.to_upper_camel_case(),
                )
            }
            // Records + enums delegate to the free functions in their owning
            // namespace's `<namespace>_codecs.hpp`. A same-namespace type is
            // unqualified (the impl file includes this namespace's codecs);
            // a foreign type is qualified with `::margelo::nitro::<ns>::` so
            // it resolves against the foreign codecs header.
            Self::Record {
                namespace,
                name: type_name,
            }
            | Self::Enum {
                namespace,
                name: type_name,
            } => format!(
                "{prefix}lower_{ty}({name})",
                prefix = Self::codec_ns_prefix(namespace, current_ns),
                ty = type_name.to_upper_camel_case(),
            ),
            // Passing an interface as an argument hands its handle to Rust,
            // which *consumes* one Arc reference. Clone first so the JS-side
            // wrapper keeps its own live reference.
            Self::Interface { .. } => format!("{name}->clone_handle()"),
            // A custom type lowers exactly as its inner builtin (it has no
            // wire identity of its own); the nominal alias / converters live in
            // the TS surface only.
            Self::Custom { inner, .. } => {
                inner.lower_expr(name, current_ns, alloc_symbol, reserve_symbol)
            }
            Self::Stub => {
                format!("::ubrn::nitro::lower_stub(/* unsupported in nitro v1 */ {name})")
            }
            _ => format!("ubrn::nitro::lower_{}({name})", self.lower_suffix()),
        }
    }

    /// Expression that lifts a `c_type`-typed value to the `cxx_type`.
    /// For RustBuffer-bearing types the caller is responsible for
    /// freeing the buffer after the lift. `current_ns` is the namespace of
    /// the file this expression is emitted into, used to decide whether a
    /// record / enum codec call needs foreign-namespace qualification.
    pub fn lift_expr(&self, name: &str, current_ns: &str) -> String {
        match self {
            Self::Bool => format!("ubrn::nitro::lift_bool({name})"),
            Self::String => format!("ubrn::nitro::lift_string({name})"),
            Self::Bytes => format!("ubrn::nitro::lift_bytes({name})"),
            Self::Timestamp => format!("ubrn::nitro::lift_timestamp({name})"),
            Self::Duration => format!("ubrn::nitro::lift_duration({name})"),
            Self::Optional(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_reader = inner.read_fn_template_arg(current_ns);
                format!(
                    "ubrn::nitro::lift_optional<{ty}, {reader}>({name})",
                    ty = inner_cxx,
                    reader = inner_reader,
                )
            }
            Self::Sequence(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_reader = inner.read_fn_template_arg(current_ns);
                format!(
                    "ubrn::nitro::lift_sequence<{ty}, {reader}>({name})",
                    ty = inner_cxx,
                    reader = inner_reader,
                )
            }
            Self::Map(k, v) => {
                let k_cxx = k.cxx_type();
                let v_cxx = v.cxx_type();
                let k_reader = k.read_fn_template_arg(current_ns);
                let v_reader = v.read_fn_template_arg(current_ns);
                format!(
                    "ubrn::nitro::lift_map<{kt}, {vt}, {kr}, {vr}>({name})",
                    kt = k_cxx,
                    vt = v_cxx,
                    kr = k_reader,
                    vr = v_reader,
                )
            }
            Self::CallbackInterface { namespace, name: cb_name } => {
                // Rust handed back an `Arc<dyn Trait>` as a u64 handle. Wrap
                // it in a Rust-backed PROXY: a handle-bearing `Hybrid<Name>`
                // whose (non-overridden) methods dispatch into Rust via the
                // trait's `fn_method_*` symbols (see the with_foreign branch
                // of `callback.{hpp,cpp}`). This only arises for `with_foreign`
                // traits — a foreign-only UDL callback interface has no Rust
                // impl that could be returned. Route through the proxy's
                // `Hybrid<Name>::adopt` choke point (not a bare `make_shared`),
                // which parks the owned handle in a move-only guard THROUGH the
                // allocation and relinquishes it only after the object exists —
                // so a throwing `make_shared` / Nitro base ctor frees the handle
                // instead of leaking the Rust-side reference (audit bug #27). The
                // class lives in the callback's owning namespace, so qualify when
                // it's foreign.
                format!(
                    "{prefix}Hybrid{cb}::adopt({name})",
                    prefix = Self::cxx_class_ns_prefix(namespace),
                    cb = cb_name.to_upper_camel_case(),
                )
            }
            // Records + enums delegate to the free functions in their owning
            // namespace's `<namespace>_codecs.hpp`; foreign types are
            // qualified so they resolve against the foreign codecs header.
            Self::Record {
                namespace,
                name: type_name,
            }
            | Self::Enum {
                namespace,
                name: type_name,
            } => format!(
                "{prefix}lift_{ty}({name})",
                prefix = Self::codec_ns_prefix(namespace, current_ns),
                ty = type_name.to_upper_camel_case(),
            ),
            Self::Interface {
                namespace,
                name: type_name,
            } => format!(
                // Route the owned Arc handle (uniffi `Arc::into_raw`, from an
                // interface-returning method / ctor) through the `Hybrid<Name>::adopt`
                // choke point rather than a bare `make_shared`: adopt parks the
                // handle in a move-only guard THROUGH the allocation and relinquishes
                // it only after the wrapper object exists, so a throwing `make_shared`
                // / Nitro base ctor frees the handle instead of leaking the Rust-side
                // strong count (audit bug #27).
                "::margelo::nitro::{}::Hybrid{}::adopt({name})",
                namespace,
                type_name.to_upper_camel_case()
            ),
            // A custom type lifts exactly as its inner builtin (no wire
            // identity of its own).
            Self::Custom { inner, .. } => inner.lift_expr(name, current_ns),
            Self::Stub => format!("::ubrn::nitro::lift_stub(/* unsupported in nitro v1 */ {name})"),
            _ => format!("ubrn::nitro::lift_{}({name})", self.lower_suffix()),
        }
    }

    /// True for types whose top-level lift can take ownership of the
    /// returned `RustBuffer` and hand its memory straight to JS (zero-copy),
    /// meaning the generated method must NOT separately free the buffer —
    /// the value's own finalizer does. Currently `Bytes`, whose payload we
    /// expose directly as the ArrayBuffer's backing store.
    pub fn lift_consumes_buffer(&self) -> bool {
        match self {
            Self::Bytes => true,
            // A custom type inherits its inner builtin's buffer-ownership
            // behavior (e.g. a `custom Blob = Bytes` consumes the buffer).
            Self::Custom { inner, .. } => inner.lift_consumes_buffer(),
            _ => false,
        }
    }

    /// Zero-copy top-level lift that consumes the `RustBuffer` (see
    /// [`Self::lift_consumes_buffer`]). `free_symbol` is the namespace
    /// `ffi_<crate>_rustbuffer_free`, wired as the ArrayBuffer's finalizer.
    pub fn lift_owning_expr(&self, name: &str, current_ns: &str, free_symbol: &str) -> String {
        match self {
            Self::Bytes => {
                format!("ubrn::nitro::lift_bytes_owning<&{free_symbol}>({name})")
            }
            // A custom type defers to its inner builtin's owning lift (the only
            // case that reaches here is a custom wrapping `Bytes`, since
            // `lift_consumes_buffer` gates this path).
            Self::Custom { inner, .. } => inner.lift_owning_expr(name, current_ns, free_symbol),
            // Only `Bytes` sets `lift_consumes_buffer`, so this is unreachable
            // for other types; fall back to the copying lift to stay total.
            _ => self.lift_expr(name, current_ns),
        }
    }

    /// Template argument used as the per-element `write_` thunk when
    /// this type appears inside a composite. Composites nest via
    /// function pointers, so the inner write/read are spelled as
    /// template arg expressions, not function calls. `current_ns` decides
    /// whether a nested record / enum thunk needs foreign qualification.
    fn write_fn_template_arg(
        &self,
        current_ns: &str,
        alloc_symbol: &str,
        reserve_symbol: &str,
    ) -> String {
        match self {
            Self::Bool => format!("&ubrn::nitro::write_bool<&{alloc_symbol}, &{reserve_symbol}>"),
            Self::String => {
                format!("&ubrn::nitro::write_string<&{alloc_symbol}, &{reserve_symbol}>")
            }
            Self::Optional(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_writer =
                    inner.write_fn_template_arg(current_ns, alloc_symbol, reserve_symbol);
                format!(
                    "&ubrn::nitro::write_optional<{ty}, &{alloc}, &{reserve}, {writer}>",
                    ty = inner_cxx,
                    alloc = alloc_symbol,
                    reserve = reserve_symbol,
                    writer = inner_writer,
                )
            }
            Self::Sequence(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_writer =
                    inner.write_fn_template_arg(current_ns, alloc_symbol, reserve_symbol);
                format!(
                    "&ubrn::nitro::write_sequence<{ty}, &{alloc}, &{reserve}, {writer}>",
                    ty = inner_cxx,
                    alloc = alloc_symbol,
                    reserve = reserve_symbol,
                    writer = inner_writer,
                )
            }
            Self::Map(k, v) => {
                let k_cxx = k.cxx_type();
                let v_cxx = v.cxx_type();
                let k_writer = k.write_fn_template_arg(current_ns, alloc_symbol, reserve_symbol);
                let v_writer = v.write_fn_template_arg(current_ns, alloc_symbol, reserve_symbol);
                format!(
                    "&ubrn::nitro::write_map<{kt}, {vt}, &{alloc}, &{reserve}, {kw}, {vw}>",
                    kt = k_cxx,
                    vt = v_cxx,
                    alloc = alloc_symbol,
                    reserve = reserve_symbol,
                    kw = k_writer,
                    vw = v_writer,
                )
            }
            Self::Bytes => {
                format!("&ubrn::nitro::write_bytes<&{alloc_symbol}, &{reserve_symbol}>")
            }
            Self::Timestamp => {
                format!("&ubrn::nitro::write_timestamp<&{alloc_symbol}, &{reserve_symbol}>")
            }
            Self::Duration => {
                format!("&ubrn::nitro::write_duration<&{alloc_symbol}, &{reserve_symbol}>")
            }
            // A callback interface inside a composite crosses as its u64
            // handle. `write_callback_handle` installs the vtable + turns the
            // `shared_ptr<Hybrid<Name>>` into the handle (register JS impl /
            // clone proxy) via the class's `ensure_vtable` / `lower_to_handle`
            // surface. The class lives in the callback's owning namespace, so
            // qualify when it's foreign.
            Self::CallbackInterface { namespace, name } => format!(
                "&ubrn::nitro::write_callback_handle<{prefix}Hybrid{name}, &{alloc_symbol}, &{reserve_symbol}>",
                prefix = Self::cxx_class_ns_prefix(namespace),
                name = name.to_upper_camel_case(),
            ),
            // Nested record / enum inside a composite delegate to the
            // per-type `write_<Name>` stream thunk emitted in
            // `<namespace>_codecs.hpp`. Those are *templated on the writer
            // type* (`template <class W> void write_<Name>(W&, const T&)`)
            // so a nested record from any namespace serializes straight into
            // the *outer* buffer's writer. We leave the writer template arg
            // implicit: the composite's `WriteInner` parameter has a fixed
            // `void (*)(Writer&, const T&)` type, so taking the function
            // template's address deduces the matching instantiation for
            // whatever writer the enclosing composite was instantiated with.
            // A foreign type's thunk is qualified with its owning namespace
            // so the symbol resolves against the foreign codecs header.
            Self::Record {
                namespace,
                name: type_name,
            }
            | Self::Enum {
                namespace,
                name: type_name,
            } => format!(
                "&{prefix}write_{ty}",
                prefix = Self::codec_ns_prefix(namespace, current_ns),
                ty = type_name.to_upper_camel_case(),
            ),
            // An interface inside a composite crosses as its u64 handle.
            // `write_interface_handle` reads `raw_handle()` off the
            // shared_ptr and writes the bare u64; lowering ownership stays
            // with the C++ side (uniffi clones on its end when it consumes
            // a handle-by-value out of a buffer).
            Self::Interface { namespace, name } => format!(
                "&ubrn::nitro::write_interface_handle<::margelo::nitro::{}::Hybrid{}>",
                namespace,
                name.to_upper_camel_case()
            ),
            // A custom type serializes exactly as its inner builtin.
            Self::Custom { inner, .. } => {
                inner.write_fn_template_arg(current_ns, alloc_symbol, reserve_symbol)
            }
            Self::Stub => "&ubrn::nitro::unsupported_compound_inside_composite".into(),
            _ => format!(
                "&ubrn::nitro::write_{}<&{alloc_symbol}, &{reserve_symbol}>",
                self.lower_suffix()
            ),
        }
    }

    /// Per-element `write_` thunk for a field inside a *record / enum stream
    /// codec*, which is templated on the writer type `W` (see `codecs.hpp`).
    /// Unlike [`Self::write_fn_template_arg`] (used by the top-level `lower_*`
    /// path, which owns a concrete `Writer<Alloc, Reserve>`), these thunks
    /// are keyed on `W` — the `*_w` overloads in `composites.hpp` — so a
    /// nested record / enum from any namespace serializes straight into the
    /// outer buffer's writer regardless of which namespace's allocator
    /// symbols that writer was built from. The literal `W` token refers to
    /// the enclosing codec template's writer parameter.
    fn codec_write_thunk_arg(&self, current_ns: &str) -> String {
        match self {
            Self::Bool => "&ubrn::nitro::write_bool_w".into(),
            Self::String => "&ubrn::nitro::write_string_w".into(),
            Self::Bytes => "&ubrn::nitro::write_bytes_w".into(),
            Self::Timestamp => "&ubrn::nitro::write_timestamp_w".into(),
            Self::Duration => "&ubrn::nitro::write_duration_w".into(),
            Self::Optional(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_writer = inner.codec_write_thunk_arg(current_ns);
                format!(
                    "&ubrn::nitro::write_optional_w<{ty}, W, {writer}>",
                    ty = inner_cxx,
                    writer = inner_writer,
                )
            }
            Self::Sequence(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_writer = inner.codec_write_thunk_arg(current_ns);
                format!(
                    "&ubrn::nitro::write_sequence_w<{ty}, W, {writer}>",
                    ty = inner_cxx,
                    writer = inner_writer,
                )
            }
            Self::Map(k, v) => {
                let k_cxx = k.cxx_type();
                let v_cxx = v.cxx_type();
                let k_writer = k.codec_write_thunk_arg(current_ns);
                let v_writer = v.codec_write_thunk_arg(current_ns);
                format!(
                    "&ubrn::nitro::write_map_w<{kt}, {vt}, W, {kw}, {vw}>",
                    kt = k_cxx,
                    vt = v_cxx,
                    kw = k_writer,
                    vw = v_writer,
                )
            }
            // Writer-type-generic callback-handle write thunk for the
            // writer-templated record / enum stream codecs (see
            // `write_fn_template_arg`'s `CallbackInterface` arm).
            Self::CallbackInterface { namespace, name } => format!(
                "&ubrn::nitro::write_callback_handle_w<{prefix}Hybrid{name}>",
                prefix = Self::cxx_class_ns_prefix(namespace),
                name = name.to_upper_camel_case(),
            ),
            Self::Record {
                namespace,
                name: type_name,
            }
            | Self::Enum {
                namespace,
                name: type_name,
            } => format!(
                "&{prefix}write_{ty}",
                prefix = Self::codec_ns_prefix(namespace, current_ns),
                ty = type_name.to_upper_camel_case(),
            ),
            Self::Interface { namespace, name } => format!(
                "&ubrn::nitro::write_interface_handle_w<::margelo::nitro::{}::Hybrid{}>",
                namespace,
                name.to_upper_camel_case()
            ),
            // A custom type's stream thunk is its inner builtin's.
            Self::Custom { inner, .. } => inner.codec_write_thunk_arg(current_ns),
            Self::Stub => "&ubrn::nitro::unsupported_compound_inside_composite".into(),
            _ => format!("&ubrn::nitro::write_{}_w", self.lower_suffix()),
        }
    }

    fn read_fn_template_arg(&self, current_ns: &str) -> String {
        match self {
            Self::Bool => "&ubrn::nitro::read_bool".into(),
            Self::String => "&ubrn::nitro::read_string".into(),
            Self::Bytes => "&ubrn::nitro::read_bytes".into(),
            Self::Timestamp => "&ubrn::nitro::read_timestamp".into(),
            Self::Duration => "&ubrn::nitro::read_duration".into(),
            Self::Optional(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_reader = inner.read_fn_template_arg(current_ns);
                format!(
                    "&ubrn::nitro::read_optional<{ty}, {reader}>",
                    ty = inner_cxx,
                    reader = inner_reader,
                )
            }
            Self::Sequence(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_reader = inner.read_fn_template_arg(current_ns);
                format!(
                    "&ubrn::nitro::read_sequence<{ty}, {reader}>",
                    ty = inner_cxx,
                    reader = inner_reader,
                )
            }
            Self::Map(k, v) => {
                let k_cxx = k.cxx_type();
                let v_cxx = v.cxx_type();
                let k_reader = k.read_fn_template_arg(current_ns);
                let v_reader = v.read_fn_template_arg(current_ns);
                format!(
                    "&ubrn::nitro::read_map<{kt}, {vt}, {kr}, {vr}>",
                    kt = k_cxx,
                    vt = v_cxx,
                    kr = k_reader,
                    vr = v_reader,
                )
            }
            // A callback interface decoded out of a composite is an
            // `Arc<dyn Trait>` Rust embedded — wrap its u64 handle in a
            // Rust-backed proxy `Hybrid<Name>` (see `lift_expr`). The class
            // lives in the callback's owning namespace, so qualify when it's
            // foreign.
            Self::CallbackInterface { namespace, name } => format!(
                "&ubrn::nitro::read_callback_proxy<{prefix}Hybrid{name}>",
                prefix = Self::cxx_class_ns_prefix(namespace),
                name = name.to_upper_camel_case(),
            ),
            // The read stream codec signature (`<Name>(RustBufferReader&)`)
            // is namespace-independent, so a foreign type's `read_<Name>`
            // nests just by fully qualifying the symbol — no writer pinning
            // needed (unlike the write side).
            Self::Record {
                namespace,
                name: type_name,
            }
            | Self::Enum {
                namespace,
                name: type_name,
            } => format!(
                "&{prefix}read_{ty}",
                prefix = Self::codec_ns_prefix(namespace, current_ns),
                ty = type_name.to_upper_camel_case(),
            ),
            Self::Interface { namespace, name } => format!(
                "&ubrn::nitro::read_interface_handle<::margelo::nitro::{}::Hybrid{}>",
                namespace,
                name.to_upper_camel_case()
            ),
            // A custom type's reader is its inner builtin's.
            Self::Custom { inner, .. } => inner.read_fn_template_arg(current_ns),
            Self::Stub => "&ubrn::nitro::unsupported_compound_inside_composite".into(),
            _ => format!("&ubrn::nitro::read_{}", self.lower_suffix()),
        }
    }

    fn lower_suffix(&self) -> &'static str {
        match self {
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            _ => "",
        }
    }

    /// Header file basenames this type pulls in transitively for its C++
    /// struct definition + `JSIConverter`. Records / enums live in
    /// `<Name>.hpp`; interfaces in `Hybrid<Name>.hpp`. Composites recurse
    /// into their element types. Primitives / string / bytes / date /
    /// duration need no extra header (they're covered by Nitro core +
    /// `<NitroUniffi.hpp>`).
    pub fn referenced_headers(&self) -> Vec<String> {
        match self {
            Self::Optional(inner) | Self::Sequence(inner) => inner.referenced_headers(),
            Self::Map(k, v) => {
                let mut out = k.referenced_headers();
                out.extend(v.referenced_headers());
                out
            }
            Self::Record { name, .. } | Self::Enum { name, .. } => {
                vec![format!("{}.hpp", name.to_upper_camel_case())]
            }
            Self::Interface { name, .. } | Self::CallbackInterface { name, .. } => {
                vec![format!("Hybrid{}.hpp", name.to_upper_camel_case())]
            }
            // A custom type pulls in whatever its inner builtin needs (the
            // alias itself has no header — it's TS-surface-only).
            Self::Custom { inner, .. } => inner.referenced_headers(),
            _ => Vec::new(),
        }
    }

    /// Collect the `(namespace, UpperCamelName)` of every record / enum this
    /// type references — by value, by `vector`/`optional`/`map`, anything that
    /// drives a `#include "<Name>.hpp"` in [`Self::referenced_headers`]. Used to
    /// build the record/enum *header* dependency graph for cycle detection
    /// (see [`NitroModule`]'s SCC pass). Interfaces / callbacks are excluded:
    /// they cross behind a `shared_ptr` and only need a forward declaration, so
    /// they never participate in a `<Name>.hpp` include cycle. Recurses through
    /// composites.
    pub fn referenced_value_types(&self, out: &mut BTreeSet<(String, String)>) {
        match self {
            Self::Optional(inner) | Self::Sequence(inner) => inner.referenced_value_types(out),
            Self::Map(k, v) => {
                k.referenced_value_types(out);
                v.referenced_value_types(out);
            }
            Self::Record { namespace, name } | Self::Enum { namespace, name } => {
                out.insert((namespace.clone(), name.to_upper_camel_case()));
            }
            // A custom type references whatever its inner builtin does.
            Self::Custom { inner, .. } => inner.referenced_value_types(out),
            _ => {}
        }
    }

    /// `(namespace, UpperCamelName)` of the record/enum this type names **by
    /// value** — i.e. directly, not behind a `vector`/`optional`/`map`. A
    /// by-value field needs its type *complete* at the struct definition, so it
    /// cannot be satisfied by a forward declaration; in a header cycle the
    /// by-value side must therefore `#include` the partner up front (only the
    /// wrapper-indirected side can forward-declare + late-include). `None` for
    /// composites, primitives, and interfaces (interfaces cross behind a
    /// `shared_ptr`, so a forward declaration always suffices).
    pub fn by_value_type(&self) -> Option<(String, String)> {
        match self {
            Self::Record { namespace, name } | Self::Enum { namespace, name } => {
                Some((namespace.clone(), name.to_upper_camel_case()))
            }
            // A custom directly wrapping a record / enum holds it by value
            // (the alias is a transparent newtype on the C++ side).
            Self::Custom { inner, .. } => inner.by_value_type(),
            _ => None,
        }
    }

    /// Headers for types this type references that must be a *complete* type
    /// at the point of use — records / enums (passed/held by value). Interface
    /// types are excluded: they always cross as `std::shared_ptr<Hybrid…>`,
    /// for which a forward declaration suffices in a header (see
    /// [`Self::referenced_interface_classes`]). Recurses through composites.
    pub fn referenced_value_headers(&self) -> Vec<String> {
        match self {
            Self::Optional(inner) | Self::Sequence(inner) => inner.referenced_value_headers(),
            Self::Map(k, v) => {
                let mut out = k.referenced_value_headers();
                out.extend(v.referenced_value_headers());
                out
            }
            Self::Record { name, .. } | Self::Enum { name, .. } => {
                vec![format!("{}.hpp", name.to_upper_camel_case())]
            }
            // Callback interfaces need the complete `Hybrid<Name>` type at the
            // use site (the lowering calls `ensure_<Name>_vtable_init()` +
            // `CallbackHandleMap<Hybrid<Name>>`), so include the full header
            // rather than forward-declaring.
            Self::CallbackInterface { name, .. } => {
                vec![format!("Hybrid{}.hpp", name.to_upper_camel_case())]
            }
            // A custom type needs whatever complete-type headers its inner
            // builtin does.
            Self::Custom { inner, .. } => inner.referenced_value_headers(),
            _ => Vec::new(),
        }
    }

    /// `(namespace, Hybrid<Name>)` for every interface this type references.
    /// Used to emit forward declarations in headers (an interface only ever
    /// appears behind a `shared_ptr` in a signature, so the header needs the
    /// class *declared*, not defined — and mutually-referential interfaces
    /// would deadlock on includes). The full header is included in the `.cpp`.
    /// Recurses through composites.
    pub fn referenced_interface_classes(&self) -> Vec<(String, String)> {
        match self {
            Self::Optional(inner) | Self::Sequence(inner) => inner.referenced_interface_classes(),
            Self::Map(k, v) => {
                let mut out = k.referenced_interface_classes();
                out.extend(v.referenced_interface_classes());
                out
            }
            Self::Interface { namespace, name } => {
                vec![(
                    namespace.clone(),
                    format!("Hybrid{}", name.to_upper_camel_case()),
                )]
            }
            // A custom type forward-declares whatever interfaces its inner
            // builtin references.
            Self::Custom { inner, .. } => inner.referenced_interface_classes(),
            _ => Vec::new(),
        }
    }

    /// Namespaces (other than `current_ns`) whose record / enum *codecs*
    /// this type's serialization reaches into. Recurses through composites.
    /// Drives the foreign `<ns>_codecs.hpp` include set so the qualified
    /// `write_/read_/lower_/lift_<Name>` calls resolve. Interfaces are
    /// excluded — their wire form is a bare u64 handle written by the
    /// header-only `write_interface_handle`, no foreign codec needed.
    pub fn foreign_codec_namespaces(&self, current_ns: &str, out: &mut BTreeSet<String>) {
        match self {
            Self::Optional(inner) | Self::Sequence(inner) => {
                inner.foreign_codec_namespaces(current_ns, out)
            }
            Self::Map(k, v) => {
                k.foreign_codec_namespaces(current_ns, out);
                v.foreign_codec_namespaces(current_ns, out);
            }
            Self::Record { namespace, .. } | Self::Enum { namespace, .. }
                if namespace != current_ns =>
            {
                out.insert(namespace.clone());
            }
            // A custom type reaches into whatever foreign codecs its inner
            // builtin does (the custom alias itself has no codec).
            Self::Custom { inner, .. } => inner.foreign_codec_namespaces(current_ns, out),
            _ => {}
        }
    }

    /// Collect the cross-namespace TS *type names* this type references,
    /// keyed by the owning foreign namespace, into `out`. Drives the
    /// `import type { … } from './<ForeignNamespaceCamel>.nitro'` block the
    /// `.nitro.ts` spec must emit so a record / enum / interface / callback
    /// from a sibling namespace resolves (the TS spec has no `export *`
    /// glue — every referenced name must be imported by its own module).
    ///
    /// Records, enums, interfaces and callbacks all surface as a named TS
    /// type whose spelling is `name.to_upper_camel_case()` (matching
    /// [`Self::ts_type`]). Composites recurse into their element types. A
    /// same-namespace type (`namespace == current_ns`) needs no import — it
    /// is declared in the same file. Primitives carry no name.
    fn foreign_ts_type_refs(
        &self,
        current_ns: &str,
        out: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        match self {
            Self::Optional(inner) | Self::Sequence(inner) => {
                inner.foreign_ts_type_refs(current_ns, out)
            }
            Self::Map(k, v) => {
                k.foreign_ts_type_refs(current_ns, out);
                v.foreign_ts_type_refs(current_ns, out);
            }
            Self::Record { namespace, name }
            | Self::Enum { namespace, name }
            | Self::Interface { namespace, name }
            | Self::CallbackInterface { namespace, name }
                if !namespace.is_empty() && namespace != current_ns =>
            {
                out.entry(namespace.clone())
                    .or_default()
                    .insert(name.to_upper_camel_case());
            }
            // A foreign custom type surfaces its own nominal alias (imported
            // from the owning namespace's spec), and its inner builtin may in
            // turn reference further foreign types (e.g. `custom T =
            // Sequence<ForeignRecord>`), so recurse through `inner` too.
            Self::Custom {
                namespace,
                name,
                inner,
            } => {
                if !namespace.is_empty() && namespace != current_ns {
                    out.entry(namespace.clone())
                        .or_default()
                        .insert(name.to_upper_camel_case());
                }
                inner.foreign_ts_type_refs(current_ns, out);
            }
            _ => {}
        }
    }

    /// Statement that serializes `<base>.<field>` into the open
    /// writer `w`, field-by-field, inside a record / enum stream codec.
    /// Every type — primitive, composite, nested record/enum, interface —
    /// funnels through the same `(writer, value)` thunk shape, so the record
    /// codec body is a uniform field walk. The stream codec is templated on
    /// the writer type `W` (so a foreign record can nest into this
    /// namespace's outer buffer), hence the writer-generic `*_w` thunks
    /// rather than the allocator-symbol-keyed ones. `current_ns` qualifies a
    /// nested foreign record / enum codec correctly.
    pub fn stream_write_stmt(&self, base: &str, field: &str, current_ns: &str) -> String {
        // The composite / nested thunks are spelled as function-pointer
        // template args (`&fn<...>`); strip the leading `&` to call them.
        let thunk = self.codec_write_thunk_arg(current_ns);
        let callee = thunk.strip_prefix('&').unwrap_or(&thunk);
        format!("{callee}(w, {base}.{field});")
    }

    /// Expression that deserializes one field of this type from the open
    /// `RustBufferReader` named `r`. Mirror of [`Self::stream_write_stmt`].
    pub fn stream_read_expr(&self, current_ns: &str) -> String {
        let thunk = self.read_fn_template_arg(current_ns);
        let callee = thunk.strip_prefix('&').unwrap_or(&thunk);
        format!("{callee}(r)")
    }

    /// Whether a value of this type can be both decoded *and* rendered into
    /// a human-readable error-message fragment via
    /// `ubrn::nitro::error_field_to_string`. Error variant payloads are
    /// surfaced to JS through the C++ exception's `what()` string (the only
    /// channel Nitro hands to `jsi::JSError`), so the field has to stringify.
    ///
    /// Scalars / bool / string / bytes / date / duration have direct
    /// `error_field_to_string` overloads. Composites of those round-trip too
    /// (their decoders exist and `error_field_to_string` recurses through the
    /// `std::optional` / `std::vector` / `std::unordered_map` overloads).
    ///
    /// Records and data-enums are now ALSO surfaced (audit bug #29): every
    /// record / enum — including an error enum used as another error's *field*,
    /// which is emitted as a value enum too (see [`NitroModule::from_general`]'s
    /// `EnumShape::Error` arm, which pushes onto BOTH `errors` and `enums`) —
    /// has a `read_<Name>` reader in `<ns>_codecs.hpp`, and the codegen emits a
    /// matching `error_field_to_string(const <Name>&)` overload (the G2 step in
    /// the fix plan), so `RootError::Complex(error=ComplexException::OsError(...))`
    /// renders into `error.message` rather than degrading to the tag-only form.
    /// This predicate MUST stay coupled with those emitted overloads.
    ///
    /// Interfaces, callbacks and stubs stay unsurfaced: an interface / callback
    /// crosses as an opaque handle with no value reader, and a `Stub` has no
    /// codec at all — those variants fall back to the tag-only message.
    pub fn is_error_message_decodable(&self) -> bool {
        match self {
            Self::Bool
            | Self::U8
            | Self::U16
            | Self::U32
            | Self::U64
            | Self::I8
            | Self::I16
            | Self::I32
            | Self::I64
            | Self::F32
            | Self::F64
            | Self::String
            | Self::Bytes
            | Self::Timestamp
            | Self::Duration => true,
            Self::Optional(inner) | Self::Sequence(inner) => inner.is_error_message_decodable(),
            Self::Map(k, v) => k.is_error_message_decodable() && v.is_error_message_decodable(),
            // A custom type stringifies exactly as its inner builtin (it's a
            // transparent newtype on the wire / for the error-message render).
            Self::Custom { inner, .. } => inner.is_error_message_decodable(),
            // A record / data-enum has a `read_<Name>` + an
            // `error_field_to_string(const <Name>&)` overload (see the doc
            // comment), so its payload renders into the error message.
            Self::Record { .. } | Self::Enum { .. } => true,
            Self::Interface { .. } | Self::CallbackInterface { .. } | Self::Stub => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReturnKind {
    Void,
    Value(NitroType),
}

impl ReturnKind {
    fn from_general(ty: Option<&general::Type>) -> Result<Self> {
        match ty {
            None => Ok(Self::Void),
            Some(t) => Ok(Self::Value(NitroType::from_type(t)?)),
        }
    }

    pub fn ts_type(&self) -> String {
        match self {
            Self::Void => "void".into(),
            Self::Value(t) => t.ts_type(),
        }
    }

    pub fn cxx_type(&self) -> String {
        match self {
            Self::Void => "void".into(),
            Self::Value(t) => t.cxx_type(),
        }
    }

    pub fn c_type(&self) -> &'static str {
        match self {
            Self::Void => "void",
            Self::Value(t) => t.c_type(),
        }
    }

    /// True when the FFI return value is a `RustBuffer` the C++ side owns
    /// and must free after lifting (strings, bytes, records, enums,
    /// optionals, sequences, maps, date/duration). Primitives, handles
    /// and `void` are not buffer-backed. The generated method body frees
    /// the buffer via the namespace `rustbuffer_free` once the lifted
    /// value has been copied out.
    pub fn returns_owned_rustbuffer(&self) -> bool {
        matches!(self, Self::Value(t) if t.c_type() == "RustBuffer")
    }
}

/// A uniffi record (UDL `dictionary` / Rust struct). ubrn emits the C++
/// struct + `JSIConverter` (`<Name>.hpp`) and the RustBuffer codec in
/// `<namespace>_codecs.hpp`.
pub struct NitroRecord {
    pub ts_name: String,
    pub fields: Vec<NitroRecordField>,
    /// Author docstring from the uniffi metadata, if any (audit bug #23).
    pub docstring: Option<String>,
    /// The other record/enum types this record forms a header `#include`
    /// cycle with — i.e. the rest of its strongly-connected component in the
    /// record/enum dependency graph (see [`NitroModule::resolve_cycles`]).
    /// Empty for the common acyclic case. When non-empty, the emitted
    /// `<Name>.hpp` switches to the cycle-safe layout (forward-declared
    /// partners, `JSIConverter` *declared* in the header and *defined*
    /// out-of-line in a guarded `<Name>.conv.hpp` footer pulled in after all
    /// the cycle's structs are complete). Sorted, excludes self.
    pub cycle_partners: Vec<CycleMember>,
}

impl NitroRecord {
    /// Sum of the leading run of fixed-wire-width fields, in bytes. Used to
    /// `reserve_additional` the RustBuffer once before the field walk in
    /// `write_<Name>` instead of growing it incrementally. Capacity-only: the
    /// wire bytes written are identical. Only the contiguous prefix of
    /// fixed-width fields is counted (a variable-width field ends the run) so
    /// the reservation never over-allocates relative to what those leading
    /// fields actually write.
    pub fn fixed_prefix_width(&self) -> usize {
        let mut total = 0usize;
        for field in &self.fields {
            match field.ty.fixed_wire_width() {
                Some(w) => total += w,
                None => break,
            }
        }
        total
    }

    fn from_general(record: &general::Record) -> Result<Self> {
        let ts_name = record.name.to_upper_camel_case();
        let fields = record
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| NitroRecordField::from_general(f, i))
            .collect();
        Ok(Self {
            ts_name,
            fields,
            docstring: record.docstring.clone(),
            cycle_partners: Vec::new(),
        })
    }

    /// Author docstring formatted as JSDoc, or `None`. See [`format_ts_docstring`].
    pub fn ds(&self) -> Option<String> {
        format_ts_docstring(self.docstring.as_deref())
    }

    /// `true` when at least one field carries a default value — gates the
    /// consumer module's runtime `create` / `defaults` factory (a record with
    /// no defaults needs no factory; every field must be supplied). See audit
    /// bug #10.
    pub fn has_defaults(&self) -> bool {
        self.fields.iter().any(|f| f.default_value.is_some())
    }

    /// `true` when this record participates in a header `#include` cycle and
    /// must use the cycle-safe emission layout.
    pub fn in_cycle(&self) -> bool {
        !self.cycle_partners.is_empty()
    }

    /// Headers this record's struct definition depends on (nested
    /// records / enums / interfaces), deduped and excluding its own — and,
    /// in the cyclic case, excluding the cycle partners (those are
    /// forward-declared and pulled in *after* the struct, by the template).
    pub fn dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.ts_name);
        let mut headers = dedup_headers(
            self.fields.iter().flat_map(|f| f.ty.referenced_headers()),
            &own,
        );
        // Drop only the *wrapper-indirected* cycle partners: those are
        // forward-declared and pulled in after the struct. By-value partners
        // stay as ordinary top includes (the struct needs them complete).
        headers.retain(|h| {
            !self
                .cycle_partners
                .iter()
                .any(|m| &m.header == h && !m.by_value)
        });
        headers
    }
}

/// One member of a record/enum header `#include` cycle (a strongly-connected
/// component of size ≥ 2). Carries the spellings the cycle-safe templates need:
/// the partner header to `#include`, its `<Name>.conv.hpp` converter footer,
/// and the `UBRN_CYC_…` preprocessor sentinels that gate out-of-line converter
/// emission until every struct in the cycle is complete.
pub struct CycleMember {
    /// Bare `UpperCamel` type name, e.g. `DbValue` — used to forward-declare
    /// the partner inside the shared namespace.
    pub name: String,
    /// `<Name>.hpp` — the partner's main header.
    pub header: String,
    /// `<Name>.conv.hpp` — the partner's guarded out-of-line converter footer.
    pub conv_header: String,
    /// `UBRN_CYC_<ns>_<Name>_STRUCT` — defined once the struct is complete.
    pub struct_sentinel: String,
    /// `true` when the *owning* type holds this partner **by value** (a direct
    /// field, not through `vector`/`optional`/`map`). A by-value partner needs
    /// its complete type at the owner's struct definition, so the owner
    /// `#include`s it up front (via `dependency_headers`) and does *not*
    /// forward-declare or late-include it. `false` when the partner is only
    /// ever wrapped — then the owner forward-declares it and pulls the header
    /// in after its own struct, breaking the include cycle. (If both directions
    /// were by-value the cycle would be genuinely unbreakable without heap
    /// indirection; uniffi's recursive shapes always route at least one
    /// direction through a `vector`/`optional`, so that does not occur here.)
    pub by_value: bool,
}

pub struct NitroRecordField {
    /// TS-facing / JS-object key name (lowerCamelCase), matching what
    /// Nitrogen names the field and what the wire-positional codec maps to a
    /// JS property. This is the spelling used for `PropNameIDCache::get(…,
    /// "<ts_name>")` and the `.nitro.ts` surface — it must NOT be mangled.
    pub ts_name: String,
    /// C++ struct-member / accessor identifier. Equal to `ts_name` except when
    /// `ts_name` collides with a C++ keyword (e.g. a Rust field literally
    /// named `else`, `class`, `new`), in which case it is suffixed with `_`
    /// (`else_`). Keeping it separate from `ts_name` means the JS surface is
    /// unchanged while the emitted C++ stays valid. See [`sanitize_cxx_ident`].
    pub cxx_name: String,
    /// Source Rust field name (snake_case) — kept for debug emission.
    #[allow(dead_code)]
    pub rust_name: String,
    pub ty: NitroType,
    /// The field's default value rendered as a TS literal (`BigInt("31")`,
    /// `undefined`, `"default-value"`, `Foo.create({})`, …), if the uniffi
    /// metadata carries one. Drives the record runtime `create`/`defaults`
    /// factory the consumer module emits (see [`NitroRecord::has_defaults`]).
    /// `None` for fields with no default — the consumer must supply them. See
    /// audit bug #10. Always `None` for enum-variant fields (a variant payload
    /// has no defaults).
    pub default_value: Option<String>,
    /// Author docstring from the uniffi metadata, if any. Emitted as JSDoc on
    /// the field in the consumer surface (see [`format_ts_docstring`]). See
    /// audit bug #23.
    pub docstring: Option<String>,
    /// `true` when the field's type is `Optional<T>` — the consumer / spec
    /// surface then spells it as an OPTIONAL PROPERTY (`name?: T`) rather than
    /// a required `name: (T) | undefined`. Cached at construction from `ty`.
    pub optional: bool,
}

impl NitroRecordField {
    fn from_general(field: &general::Field, index: usize) -> Self {
        // Tuple / positional fields (Rust `enum V { A(String) }` or a tuple
        // struct) carry no usable name, so `to_lower_camel_case` yields "".
        // Synthesize a stable `v<index>` so the C++ struct member and the JS
        // object key are both valid and agree (the wire codec is positional,
        // so the name is purely a surface detail — it just has to be
        // consistent between the struct, the JSIConverter, and the .nitro.ts).
        let camel = field.name.to_lower_camel_case();
        let ts_name = if camel.is_empty() {
            format!("v{index}")
        } else {
            camel
        };
        let cxx_name = sanitize_cxx_ident(&ts_name);
        let ty = NitroType::from_type_lossy(&field.ty.ty);
        let optional = matches!(ty, NitroType::Optional(_));
        Self {
            ts_name,
            cxx_name,
            rust_name: field.name.clone(),
            ty,
            default_value: field.default.as_ref().map(render_default_value),
            docstring: field.docstring.clone(),
            optional,
        }
    }

    /// Author docstring formatted as JSDoc, or `None`. See [`format_ts_docstring`].
    pub fn ds(&self) -> Option<String> {
        format_ts_docstring(self.docstring.as_deref())
    }

    /// Whether this field is surfaced as an OPTIONAL PROPERTY (`name?: T`) in
    /// the record / variant TS type — true exactly when the field type is
    /// `Optional<T>`. Pairs with [`Self::ts_field_type`] (which unwraps the one
    /// `Optional`). Record/variant-field position ONLY — [`NitroType::ts_type`]
    /// in arg / return position is unchanged. See audit bug #9.
    pub fn ts_is_optional(&self) -> bool {
        self.optional
    }

    /// The TS type spelled for this field in record / variant-field position:
    /// the inner type with ONE level of `Optional` unwrapped when
    /// [`Self::ts_is_optional`] (the `?:` carries the optionality), else the
    /// plain `ty.ts_type()`. See audit bug #9.
    pub fn ts_field_type(&self) -> String {
        match &self.ty {
            NitroType::Optional(inner) => inner.ts_type(),
            other => other.ts_type(),
        }
    }
}

/// A uniffi enum. Flat enums (no associated data) become TS string
/// unions and serialize as `i32` ordinals. Tagged enums become TS
/// discriminated unions; codec emission for the tagged case is a
/// future scope and produces a runtime-throwing body.
pub struct NitroEnum {
    pub ts_name: String,
    pub variants: Vec<NitroEnumVariant>,
    pub flat: bool,
    /// Author docstring from the uniffi metadata, if any (audit bug #23).
    pub docstring: Option<String>,
    /// The other record/enum types this enum forms a header `#include` cycle
    /// with — its SCC partners. See [`NitroRecord::cycle_partners`]. Always
    /// empty for flat enums (no payloads, so they reference nothing).
    pub cycle_partners: Vec<CycleMember>,
}

impl NitroEnum {
    fn from_general(en: &general::Enum) -> Result<Self> {
        let ts_name = en.name.to_upper_camel_case();
        let variants = en
            .variants
            .iter()
            .map(NitroEnumVariant::from_general)
            .collect();
        // NOTE: an explicit `#[repr]` discriminant (`en.meta_discr_type`) is
        // deliberately NOT carried. The flat enum is emitted as a STRING-valued
        // `export enum X { Dog = 'Dog' }` (locked decision) because Nitro's
        // flat-enum JSIConverter hashes the variant NAME — a numeric value would
        // break `canConvert`, and the wire form is the C++-computed 1-based
        // ordinal regardless. So the explicit discriminant has no runtime role
        // on the Nitro surface and is dropped rather than carried dead (#15/#16).
        Ok(Self {
            ts_name,
            variants,
            flat: en.is_flat,
            docstring: en.docstring.clone(),
            cycle_partners: Vec::new(),
        })
    }

    /// Author docstring formatted as JSDoc, or `None`. See [`format_ts_docstring`].
    pub fn ds(&self) -> Option<String> {
        format_ts_docstring(self.docstring.as_deref())
    }

    /// `true` when this enum participates in a header `#include` cycle and
    /// must use the cycle-safe emission layout.
    pub fn in_cycle(&self) -> bool {
        !self.cycle_partners.is_empty()
    }

    /// `true` when at least one variant carries payload fields (an `inner`
    /// object on the JS side). Templates use this to gate emission of the
    /// hoisted `inner` PropNameID alias so it isn't declared-but-unused (which
    /// would trip `-Wall -Werror`) for all-fieldless tagged enums.
    pub fn any_variant_has_fields(&self) -> bool {
        self.variants.iter().any(|v| !v.fields.is_empty())
    }

    /// Headers this enum's payload structs depend on, deduped and
    /// excluding its own (a recursive enum references itself, which is
    /// handled by in-file forward declaration, not an include) — and, in the
    /// cyclic case, excluding the cycle partners (forward-declared and pulled
    /// in after the struct by the template).
    pub fn dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.ts_name);
        let mut headers = dedup_headers(
            self.variants
                .iter()
                .flat_map(|v| v.fields.iter())
                .flat_map(|f| f.ty.referenced_headers()),
            &own,
        );
        // Drop only the wrapper-indirected cycle partners (forward-declared +
        // late-included); by-value partners stay as ordinary top includes.
        headers.retain(|h| {
            !self
                .cycle_partners
                .iter()
                .any(|m| &m.header == h && !m.by_value)
        });
        headers
    }
}

/// Make `name` safe to use as a C++ identifier by appending `_` when it
/// collides with a reserved keyword (C++20 keywords + alternative tokens +
/// `override`/`final` which are context-sensitive but reserved here because
/// the generated method bodies use them as virtual-override declarations) OR
/// with a `margelo::nitro::HybridObject` base-class virtual method name.
///
/// Applied ONLY to the C++-facing identifier (`cxx_name`); the JS-facing
/// `ts_name` — and the string passed to `registerHybridMethod` — keep the
/// original spelling so the JS surface is unchanged. A uniffi method named
/// `delete` thus emits `Hybrid::delete_(...)` while staying registered as
/// `"delete"`.
///
/// The `HybridObject` reservation prevents a uniffi method whose name maps to
/// a base virtual (e.g. Rust `to_string` -> `toString`, or `equals`/`dispose`)
/// from silently re-declaring that virtual on the generated subclass. Without
/// the suffix, a same-signature collision (`toString`) becomes a true vtable
/// override — hijacking the framework's own C++ `hybrid->toString()` debug
/// path so it dispatches into a Rust FFI call that can throw — and a
/// different-signature collision (`equals`/`dispose`) becomes a hidden
/// `-Woverloaded-virtual` overload. Suffixing the C++ name (`toString_`) keeps
/// the base virtuals intact for Nitro's internal use while the uniffi method
/// is still reachable from JS under its original name via the derived
/// prototype (Nitro keys prototypes by the derived type, so the JS-level
/// shadow of the same-named base method is intentional and unaffected).
fn sanitize_cxx_ident(name: &str) -> String {
    // C++20 keyword set (incl. alternative-token operator keywords and the
    // context-sensitive identifiers we treat as reserved). Kept exhaustive so
    // any uniffi name that happens to be a C++ keyword degrades to `<name>_`
    // rather than failing to compile.
    const CXX_KEYWORDS: &[&str] = &[
        "alignas",
        "alignof",
        "and",
        "and_eq",
        "asm",
        "atomic_cancel",
        "atomic_commit",
        "atomic_noexcept",
        "auto",
        "bitand",
        "bitor",
        "bool",
        "break",
        "case",
        "catch",
        "char",
        "char8_t",
        "char16_t",
        "char32_t",
        "class",
        "compl",
        "concept",
        "const",
        "consteval",
        "constexpr",
        "constinit",
        "const_cast",
        "continue",
        "co_await",
        "co_return",
        "co_yield",
        "decltype",
        "default",
        "delete",
        "do",
        "double",
        "dynamic_cast",
        "else",
        "enum",
        "explicit",
        "export",
        "extern",
        "false",
        "final",
        "float",
        "for",
        "friend",
        "goto",
        "if",
        "inline",
        "int",
        "long",
        "mutable",
        "namespace",
        "new",
        "noexcept",
        "not",
        "not_eq",
        "nullptr",
        "operator",
        "or",
        "or_eq",
        "override",
        "private",
        "protected",
        "public",
        "reflexpr",
        "register",
        "reinterpret_cast",
        "requires",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "static_assert",
        "static_cast",
        "struct",
        "switch",
        "synchronized",
        "template",
        "this",
        "thread_local",
        "throw",
        "true",
        "try",
        "typedef",
        "typeid",
        "typename",
        "union",
        "unsigned",
        "using",
        "virtual",
        "void",
        "volatile",
        "wchar_t",
        "while",
        "xor",
        "xor_eq",
    ];
    // Virtual (or JS-registered) methods declared by `margelo::nitro::
    // HybridObject` that a generated subclass must not re-declare. Names are
    // the C++/JS method spellings (lowerCamelCase, post-`to_lower_camel_case`)
    // — `toString`/`equals`/`dispose` are `virtual`, `getName` backs the
    // registered `name` getter. Sourced from
    // `react-native-nitro-modules` `cpp/core/HybridObject.{hpp,cpp}`.
    const NITRO_HYBRIDOBJECT_METHODS: &[&str] = &[
        "toString",
        "equals",
        "dispose",
        "getName",
    ];
    if CXX_KEYWORDS.contains(&name) || NITRO_HYBRIDOBJECT_METHODS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// Make `name` safe to use as a TS *parameter* identifier by appending `_`
/// when it collides with a reserved word that is illegal as a parameter
/// name.
///
/// The load-bearing case is `this`: a function parameter literally named
/// `this` is parsed by TS as the special *this-type annotation*, not a
/// real parameter — so `fn(this: T, x: U)` is seen as a one-arg function,
/// and the generated call site `fn(this, x)` then fails with `TS2554
/// Expected 1 arguments, but got 2`. (A Rust free function with a value
/// parameter literally named `this` — the `resolver_ext_*` "extension
/// method" idiom — hits exactly this.) The other strict-mode reserved
/// words are guarded defensively so any future uniffi arg name that lands
/// on one degrades to `<name>_` rather than producing invalid TS.
///
/// Applied ONLY to the TS-facing `ts_name`; the FFI lowering is positional
/// and never reads this identifier, so renaming it is purely cosmetic on
/// the wire. The renamed local is forwarded at the call site, keeping
/// arity correct.
fn sanitize_ts_arg_ident(name: &str) -> String {
    // `this` is the only one that silently changes a function's *arity*;
    // the rest are strict-mode reserved words that are outright invalid as
    // a binding identifier. Kept small + targeted — a uniffi arg name is
    // already lowerCamelCase, so most JS keywords (e.g. `class`, `for`)
    // can't appear, but the value-position reserved words below can.
    if TS_RESERVED_IDENTS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// TS reserved words that are illegal as a value-position binding identifier
/// (parameter or declaration name). Shared by [`sanitize_ts_arg_ident`] (arg
/// identifiers) and [`ts_fn_name`] (JS-facing function/method/ctor names) so
/// both apply one source of truth. `this` is the load-bearing case for args
/// (it silently changes a function's arity); the rest are strict-mode reserved
/// words. For function names the relevant ones are `void`/`function`/etc. — a
/// uniffi name like `async fn void()` would otherwise emit `export function
/// void()`, a hard TS1359 syntax error.
const TS_RESERVED_IDENTS: &[&str] = &[
    "this", "arguments", "eval", "default", "function", "in", "instanceof", "new", "return",
    "typeof", "void", "delete", "yield", "await",
];

/// Make `name` safe as a JS-facing function/method/constructor identifier.
///
/// `new` is returned unchanged here: it is the conventional primary-constructor
/// name and the consumer-module class template maps it to the JS `constructor`
/// (it never reaches a declaration site as the literal name `new`), so it must
/// NOT be rewritten — unlike in [`sanitize_ts_arg_ident`], where a *parameter*
/// literally named `new` is illegal and is suffixed.
///
/// For every other name we lowerCamelCase (matching the rest of the JS-facing
/// surface) and, if the result collides with a TS reserved word
/// ([`TS_RESERVED_IDENTS`]), suffix `_` (so `async fn void` -> `void_`). Only
/// the JS-facing `ts_name` is rewritten; `cxx_name` and `uniffi_symbol` are
/// left untouched (the FFI wire is positional / symbol-keyed and never reads
/// this identifier).
fn ts_fn_name(name: &str) -> String {
    if name == "new" {
        return name.to_string();
    }
    let camel = name.to_lower_camel_case();
    if TS_RESERVED_IDENTS.contains(&camel.as_str()) {
        format!("{camel}_")
    } else {
        camel
    }
}

/// Render a uniffi `DefaultValue` as a TS literal for the consumer module's
/// record-defaults factory. Nitro-local mirror of gen_typescript's
/// `render_default_value` / `render_literal` (`api_module/builders.rs`), with
/// two Nitro-specific spellings: 64-bit integers become `BigInt("N")` (Nitro
/// surfaces `u64`/`i64` as `bigint`) and bare `Bytes` defaults become a fresh
/// `ArrayBuffer` (Nitro's `Vec<u8>` surface is `ArrayBuffer`, not `Uint8Array`).
/// See audit bug #10.
fn render_default_value(dv: &general::DefaultValue) -> String {
    match dv {
        general::DefaultValue::Literal(lit_node) => render_literal(&lit_node.lit),
        general::DefaultValue::Default(tn) => render_type_default(&tn.ty),
    }
}

/// Render an explicit `Literal` default. Mirrors gen_typescript's
/// `render_literal`; see [`render_default_value`].
fn render_literal(lit: &general::Literal) -> String {
    match lit {
        general::Literal::Boolean(b) => b.to_string(),
        general::Literal::String(s) => format!("\"{s}\""),
        general::Literal::Int(n, _, type_node) => match &type_node.ty {
            general::Type::Int64 | general::Type::UInt64 => format!("BigInt(\"{n}\")"),
            _ => n.to_string(),
        },
        general::Literal::UInt(n, _, type_node) => match &type_node.ty {
            general::Type::Int64 | general::Type::UInt64 => format!("BigInt(\"{n}\")"),
            _ => n.to_string(),
        },
        general::Literal::Float(s, _) => s.clone(),
        general::Literal::Enum(variant, type_node) => {
            // No containing enum context here, so use the bare type name +
            // UpperCamelCase variant (matching gen_typescript's fallback).
            let type_name = match &type_node.ty {
                general::Type::Enum { name, .. } | general::Type::Custom { name, .. } => {
                    name.to_upper_camel_case()
                }
                other => NitroType::from_type_lossy(other).ts_type(),
            };
            format!("{type_name}.{}", variant.to_upper_camel_case())
        }
        general::Literal::EmptySequence => "[]".into(),
        general::Literal::EmptyMap => "new Map()".into(),
        general::Literal::None => "undefined".into(),
        general::Literal::Some { inner } => render_default_value(inner),
    }
}

/// Render the natural zero-value for a bare `default` keyword (no explicit
/// literal). Mirrors gen_typescript's `render_type_default`, adjusted for
/// Nitro's surface (`bigint`/`ArrayBuffer`); see [`render_default_value`].
fn render_type_default(ty: &general::Type) -> String {
    match ty {
        general::Type::UInt8
        | general::Type::UInt16
        | general::Type::UInt32
        | general::Type::Int8
        | general::Type::Int16
        | general::Type::Int32
        | general::Type::Float32
        | general::Type::Float64 => "0".into(),
        general::Type::UInt64 | general::Type::Int64 => "BigInt(0)".into(),
        general::Type::Boolean => "false".into(),
        general::Type::String => "\"\"".into(),
        general::Type::Bytes => "new ArrayBuffer(0)".into(),
        general::Type::Optional { .. } => "undefined".into(),
        general::Type::Sequence { .. } => "[]".into(),
        general::Type::Map { .. } => "new Map()".into(),
        general::Type::Custom { builtin, .. } => render_type_default(builtin),
        general::Type::Record { name, .. } => {
            format!("{}.create({{}})", name.to_upper_camel_case())
        }
        // No clear default semantic (matching gen_typescript): fall back to
        // `undefined` so strict-mode TS surfaces the gap.
        general::Type::Enum { .. }
        | general::Type::Interface { .. }
        | general::Type::CallbackInterface { .. }
        | general::Type::Timestamp
        | general::Type::Duration => "undefined".into(),
    }
}

/// Format an author docstring as a JSDoc `/** … */` block for the consumer /
/// spec surface. Nitro-local mirror of gen_typescript's `format_docstring`
/// (`api_module/docstring.rs`): dedent the source, prefix each line with ` * `,
/// and wrap. Returns `None` for `None` / all-whitespace docstrings so the
/// templates can fall back to their existing boilerplate. See audit bug #23.
pub fn format_ts_docstring(docstring: Option<&str>) -> Option<String> {
    let ds = docstring?;
    if ds.trim().is_empty() {
        return None;
    }
    let middle = textwrap::indent(&textwrap::dedent(ds), " * ");
    Some(format!("/**\n{middle}\n */"))
}

/// Dedup + sort a header-name iterator, dropping `own` (a type never
/// includes its own header — recursion is handled by forward declaration).
fn dedup_headers(headers: impl Iterator<Item = String>, own: &str) -> Vec<String> {
    let mut set: BTreeSet<String> = headers.collect();
    set.remove(own);
    set.into_iter().collect()
}

/// Tarjan's strongly-connected-components over the directed graph `(nodes,
/// adj)`. Returns one `Vec<String>` per SCC, each sorted, with the SCC list
/// itself sorted by first member — fully deterministic so codegen output is
/// stable across runs. A single node with no self-loop comes back as its own
/// singleton SCC; only SCCs of size ≥ 2 are genuine cycles (the caller
/// filters). Iterative (explicit stack) so a deep dependency chain can't blow
/// the native stack.
fn strongly_connected_components(
    nodes: &BTreeSet<String>,
    adj: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<Vec<String>> {
    let mut index_of: BTreeMap<String, usize> = BTreeMap::new();
    let mut lowlink: BTreeMap<String, usize> = BTreeMap::new();
    let mut on_stack: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    let mut next_index = 0usize;
    let mut sccs: Vec<Vec<String>> = Vec::new();

    // Explicit DFS frame: the node plus an iterator position over its
    // successors.
    struct Frame {
        node: String,
        succ: Vec<String>,
        pos: usize,
    }

    for root in nodes {
        if index_of.contains_key(root) {
            continue;
        }
        let mut call_stack: Vec<Frame> = vec![Frame {
            node: root.clone(),
            succ: adj
                .get(root)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default(),
            pos: 0,
        }];
        index_of.insert(root.clone(), next_index);
        lowlink.insert(root.clone(), next_index);
        next_index += 1;
        stack.push(root.clone());
        on_stack.insert(root.clone());

        while let Some(frame) = call_stack.last_mut() {
            if frame.pos < frame.succ.len() {
                let w = frame.succ[frame.pos].clone();
                frame.pos += 1;
                if !index_of.contains_key(&w) {
                    // Descend into the unvisited successor.
                    index_of.insert(w.clone(), next_index);
                    lowlink.insert(w.clone(), next_index);
                    next_index += 1;
                    stack.push(w.clone());
                    on_stack.insert(w.clone());
                    let succ = adj
                        .get(&w)
                        .map(|s| s.iter().cloned().collect())
                        .unwrap_or_default();
                    call_stack.push(Frame {
                        node: w,
                        succ,
                        pos: 0,
                    });
                } else if on_stack.contains(&w) {
                    let v = frame.node.clone();
                    let low = lowlink[&v].min(index_of[&w]);
                    lowlink.insert(v, low);
                }
            } else {
                // Done with this node — pop and, if it's an SCC root, peel.
                let v = frame.node.clone();
                call_stack.pop();
                if lowlink[&v] == index_of[&v] {
                    let mut comp: Vec<String> = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack.remove(&w);
                        comp.push(w.clone());
                        if w == v {
                            break;
                        }
                    }
                    comp.sort();
                    sccs.push(comp);
                }
                // Propagate lowlink to the parent frame.
                if let Some(parent) = call_stack.last() {
                    let p = parent.node.clone();
                    let low = lowlink[&p].min(lowlink[&v]);
                    lowlink.insert(p, low);
                }
            }
        }
    }

    sccs.sort_by(|a, b| a.first().cmp(&b.first()));
    sccs
}

pub struct NitroEnumVariant {
    /// UpperCamelCase variant name as it appears in the TS union literal.
    /// This is ALSO the runtime `tag` discriminant value: the standard
    /// uniffi tagged-enum shape keys a value as
    /// `{ tag: '<UpperCamelVariantName>', inner: <payload> }`, with the tag
    /// value being the variant name (`to_upper_camel_case`d) — so the Nitro
    /// surface must agree byte-for-byte with what the NAPI/JSI backends emit.
    pub ts_name: String,
    /// `true` when the variant's fields are positional (a Rust tuple variant,
    /// `V(T0, T1)`), `false` for named-field or unit variants. Drives the
    /// `inner` payload shape: tuple variants surface `inner` as a positional
    /// array (`Readonly<[T0, T1]>`), named variants as an object
    /// (`Readonly<{ field: T }>`) — mirroring the standard `variant_inner_type`.
    pub has_nameless_fields: bool,
    /// Associated-data fields, if any. Empty for unit variants.
    pub fields: Vec<NitroRecordField>,
    /// Author docstring from the uniffi metadata, if any (audit bug #23).
    pub docstring: Option<String>,
}

impl NitroEnumVariant {
    /// Per-variant C++ payload struct name, e.g. `Tree_Node`. One struct
    /// per variant holds that variant's fields; the enum itself is a
    /// `std::variant` over these. Unit variants get an empty struct.
    pub fn cxx_struct_name(&self, enum_name: &str) -> String {
        format!("{}_{}", enum_name.to_upper_camel_case(), self.ts_name)
    }

    /// Author docstring formatted as JSDoc, or `None`. See [`format_ts_docstring`].
    pub fn ds(&self) -> Option<String> {
        format_ts_docstring(self.docstring.as_deref())
    }

    /// True when, as an *error* variant, this variant carries fields that we
    /// can decode off the wire and render into the exception message that
    /// reaches JS via `error.message`. Requires at least one field and every
    /// field to be [`NitroType::is_error_message_decodable`] — see that
    /// method for why complex / error-enum fields fall back to tag-only.
    pub fn error_fields_surfaced(&self) -> bool {
        !self.fields.is_empty()
            && self
                .fields
                .iter()
                .all(|f| f.ty.is_error_message_decodable())
    }
}

impl NitroEnumVariant {
    fn from_general(variant: &general::Variant) -> Self {
        Self {
            ts_name: variant.name.to_upper_camel_case(),
            has_nameless_fields: matches!(variant.fields_kind, general::FieldsKind::Unnamed),
            fields: variant
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| NitroRecordField::from_general(f, i))
                .collect(),
            docstring: variant.docstring.clone(),
            // NOTE: an explicit `#[repr]` discriminant (`variant.meta_discr`)
            // is NOT carried — the flat-enum `export enum X { Dog = 'Dog' }`
            // form uses the variant NAME as the value (Nitro's converter hashes
            // the name; a numeric value would break it), so the explicit repr
            // has no runtime role on the Nitro surface. See #15/#16.
        }
    }
}

/// A uniffi error enum (UDL `[Error]` or Rust `#[derive(Error)]`).
/// Codegen for errors is a future scope — they become C++ exception
/// classes plus `lift_<Name>Error` decoders that map the variant
/// ordinal back to the typed exception. For now we capture enough
/// metadata (name + variants) so templates can iterate `module.errors`
/// without panicking, but no codec body is emitted.
pub struct NitroError {
    pub ts_name: String,
    pub variants: Vec<NitroEnumVariant>,
    /// Author docstring from the uniffi metadata, if any (audit bug #23).
    pub docstring: Option<String>,
}

impl NitroError {
    /// Author docstring formatted as JSDoc, or `None`. See [`format_ts_docstring`].
    pub fn ds(&self) -> Option<String> {
        format_ts_docstring(self.docstring.as_deref())
    }

    /// C++ exception class name we emit (matches [`NitroErrorRef::cxx_class`]).
    /// `<base>Exception` with a trailing `Error` stripped — see
    /// [`error_cxx_class`] / audit bug #28.
    pub fn cxx_class(&self) -> String {
        error_cxx_class(&self.ts_name)
    }
    /// Lifter free-function name (matches [`NitroErrorRef::lift_fn`] in the
    /// same namespace). Derived from [`Self::cxx_class`] so they stay coupled.
    pub fn lift_fn(&self) -> String {
        format!("lift_{}", self.cxx_class())
    }
    // NOTE: no `lower_fn` — a typed `code=1` sync-callback encode path is
    // Nitro-impossible (audit E1: `HybridFunction` collapses a JS throw to a
    // string-only `jsi::JSError`, so there is no typed buffer to lower into).
    // Typed errors are surfaced via the message-prefix + `<Err>_Tags.tagOf`
    // discriminator instead. The error's `flat` bit is likewise unused — the
    // `<Err>_Tags` discriminator is emitted for every error regardless of
    // shape — so it is not carried.
}

impl NitroError {
    fn from_general(en: &general::Enum) -> Result<Self> {
        let ts_name = en.name.to_upper_camel_case();
        let variants = en
            .variants
            .iter()
            .map(NitroEnumVariant::from_general)
            .collect();
        Ok(Self {
            ts_name,
            variants,
            docstring: en.docstring.clone(),
        })
    }
}

/// A uniffi `custom` type (newtype). Surfaces in the consumer module as a
/// nominal `export type <Name> = <ts_alias>` plus — when configured — the
/// `intoCustom` / `fromCustom` conversion wrappers. The wire form is the inner
/// builtin's (see [`NitroType::Custom`]), so this carries no codec emission.
/// See audit bug #17.
pub struct NitroCustom {
    /// UpperCamelCase TS name (the nominal alias).
    pub ts_name: String,
    /// The inner builtin, kept so the templates can spell the alias's RHS
    /// (`export type <Name> = <inner.ts_type()>`) and so the codec / header
    /// machinery has the underlying type if it ever needs it.
    pub inner: NitroType,
    /// The configured TS conversion, if the per-crate `uniffi.toml`
    /// `[bindings.typescript.customTypes.<Name>]` declared one. `None` means a
    /// plain structural newtype (the alias is the bare inner type and no
    /// conversion wrapper is emitted). When `Some`, the wrapper free-funcs /
    /// methods apply `into_custom` / `from_custom`.
    pub conversion: Option<NitroCustomConversion>,
}

/// A configured custom-type conversion, lifted from the TS `CustomTypeConfig`
/// (`intoCustom` / `fromCustom` / `typeName` / `imports`). The expr strings use
/// `{}` as the value placeholder, exactly like the JSI backend's
/// `CustomTypeConfig::lift` / `lower`.
pub struct NitroCustomConversion {
    /// The concrete TS type the custom value is presented as on the consumer
    /// surface (the configured `typeName`, e.g. `URL`), if any. `None` falls
    /// back to the inner builtin's TS spelling.
    pub type_name: Option<String>,
    /// `intoCustom` expression template (`{}` = the lowered/builtin value),
    /// applied when lifting a value out of the FFI into the custom type.
    pub into_custom: String,
    /// `fromCustom` expression template (`{}` = the custom value), applied when
    /// lowering a custom value into the builtin form for the FFI.
    pub from_custom: String,
    /// `(import-name, module)` pairs the configured conversion needs in scope.
    pub imports: Vec<(String, String)>,
}

impl NitroCustomConversion {
    /// Lift a wire/inner value into the presented custom type — the
    /// `intoCustom` template with `{}` substituted by `value`. Applied to a
    /// value coming *out* of the Nitro singleton (whose spec speaks the inner
    /// builtin) so the consumer surface presents the configured `typeName`.
    /// Mirrors the JSI backend's `CustomTypeConfig::lift`.
    pub fn lift(&self, value: &str) -> String {
        self.into_custom.replace("{}", value)
    }

    /// Lower a presented custom value into the wire/inner builtin — the
    /// `fromCustom` template with `{}` substituted by `value`. Applied to an
    /// argument *before* handing it to the Nitro singleton. Mirrors the JSI
    /// backend's `CustomTypeConfig::lower`.
    pub fn lower(&self, value: &str) -> String {
        self.from_custom.replace("{}", value)
    }
}

impl NitroCustom {
    fn from_general(custom: &general::CustomType, config: &TsConfig) -> Result<Self> {
        let ts_name = custom.name.to_upper_camel_case();
        let inner = NitroType::from_type(&custom.builtin.ty)?;
        // The config keys custom types by their UDL name; match on the
        // UpperCamelCase spelling (the same `ts_name`) so a `[bindings.
        // typescript.customTypes.<Name>]` entry binds. Absent => no conversion.
        //
        // A conversion is only attached when the inner builtin has a plain JS
        // runtime value form ([`NitroType::inner_supports_js_conversion`]). A
        // custom over a Record / Enum / Interface surfaces type-only under
        // Nitro, so a JSI-style conversion expr that constructs it (e.g. `new
        // MyEnum.A(v)`) would not resolve — we drop the conversion and fall
        // back to the plain inner-type alias (documented Nitro limit). See #17.
        let conversion = if inner.inner_supports_js_conversion() {
            config
                .custom_types
                .get(&ts_name)
                .map(|c| NitroCustomConversion {
                    type_name: c.type_name.clone(),
                    into_custom: c.into_custom.clone(),
                    from_custom: c.from_custom.clone(),
                    imports: c.imports.clone(),
                })
        } else {
            None
        };
        Ok(Self {
            ts_name,
            inner,
            conversion,
        })
    }

    /// The TS type the consumer surface presents this custom value as: the
    /// configured `typeName` when set, else the inner builtin's spelling. The
    /// alias declaration is `export type <ts_name> = <ts_alias_type()>`.
    pub fn ts_alias_type(&self) -> String {
        match self.conversion.as_ref().and_then(|c| c.type_name.as_ref()) {
            Some(name) => name.clone(),
            None => self.inner.ts_type(),
        }
    }
}

/// Parse the per-crate `uniffi.toml` `[bindings.typescript]` section out of the
/// namespace's captured `config_toml`, mirroring `cli.rs`'s `extract_ts_config`
/// (same `bindings.typescript` / `js` / `ts` aliasing). A namespace without a
/// config yields the default (no configured custom conversions). Kept local to
/// gen_nitro so [`NitroModule::from_general`] needs no extra parameter and we
/// don't depend on a private cli function.
fn extract_nitro_ts_config(namespace: &general::Namespace) -> Result<TsConfig> {
    #[derive(Default, serde::Deserialize)]
    struct BindingsSection {
        #[serde(default, alias = "javascript", alias = "js", alias = "ts")]
        typescript: TsConfig,
    }
    #[derive(Default, serde::Deserialize)]
    struct ConfigRoot {
        #[serde(default)]
        bindings: BindingsSection,
    }
    let Some(ref config_toml) = namespace.config_toml else {
        return Ok(TsConfig::default());
    };
    let root: ConfigRoot = toml::from_str(config_toml)?;
    Ok(root.bindings.typescript)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_arg_is_sanitized_to_avoid_ts_this_type_annotation() {
        // A Rust free fn with a value parameter named `this` (the
        // `resolver_ext_*` extension-method idiom) must NOT surface as a TS
        // `this:` parameter — that's a this-type annotation, not a real
        // arg, and would drop the function's arity by one.
        assert_eq!(sanitize_ts_arg_ident("this"), "this_");
    }

    #[test]
    fn ordinary_arg_names_pass_through_unchanged() {
        assert_eq!(sanitize_ts_arg_ident("target"), "target");
        assert_eq!(sanitize_ts_arg_ident("op"), "op");
        assert_eq!(sanitize_ts_arg_ident("otherSource"), "otherSource");
    }

    #[test]
    fn other_reserved_arg_words_are_guarded() {
        // Strict-mode reserved words that are illegal as binding
        // identifiers degrade to `<name>_` rather than emitting invalid TS.
        for w in ["arguments", "eval", "default", "function", "yield", "await"] {
            assert_eq!(sanitize_ts_arg_ident(w), format!("{w}_"));
        }
    }

    #[test]
    fn cxx_keywords_are_suffixed() {
        // A uniffi method/field whose name is a C++ keyword degrades to
        // `<name>_` so the emitted C++ stays valid.
        assert_eq!(sanitize_cxx_ident("delete"), "delete_");
        assert_eq!(sanitize_cxx_ident("union"), "union_");
        assert_eq!(sanitize_cxx_ident("else"), "else_");
    }

    #[test]
    fn nitro_hybridobject_base_methods_are_suffixed() {
        // A uniffi method whose camelCased name collides with a Nitro
        // `HybridObject` base virtual must be suffixed on the C++ side so it
        // does not silently override (`toString`) or overload
        // (`equals`/`dispose`) the framework method. The JS registration name
        // (`ts_name`) is unaffected — only `cxx_name` flows through here.
        for m in ["toString", "equals", "dispose", "getName"] {
            assert_eq!(sanitize_cxx_ident(m), format!("{m}_"));
        }
    }

    #[test]
    fn ordinary_method_names_pass_through_cxx_unchanged() {
        // Names that collide with neither a C++ keyword nor a Nitro base
        // method are emitted verbatim — including `toStr`, which is distinct
        // from the reserved `toString`.
        for m in ["toStr", "isNode", "timestampMs", "asBytes", "connect"] {
            assert_eq!(sanitize_cxx_ident(m), m);
        }
    }

    fn callback_method(is_async: bool, return_kind: ReturnKind) -> NitroCallbackMethod {
        NitroCallbackMethod {
            ts_name: "onThing".into(),
            cxx_name: "onThing".into(),
            args: Vec::new(),
            return_kind,
            is_async,
            uniffi_symbol: None,
            throws: None,
            docstring: None,
        }
    }

    #[test]
    fn async_callback_method_return_wraps_in_promise() {
        // An `async fn` foreign-trait method must surface as `Promise<T>`
        // so the JS impl can be `async`; a sync method stays the bare type.
        let m = callback_method(true, ReturnKind::Value(NitroType::Bool));
        assert_eq!(m.ts_return_signature(), "Promise<boolean>");

        let m = callback_method(true, ReturnKind::Void);
        assert_eq!(m.ts_return_signature(), "Promise<void>");
    }

    #[test]
    fn sync_callback_method_return_is_bare() {
        let m = callback_method(false, ReturnKind::Value(NitroType::Bool));
        assert_eq!(m.ts_return_signature(), "boolean");
    }

    #[test]
    fn async_callback_method_cxx_return_wraps_in_foreign_async_result() {
        // The C++ HybridObject method for an async foreign-trait method must
        // return `std::shared_ptr<ForeignAsyncResult<T>>`: the wrapper keeps
        // Nitro's `std::function` converter on the SyncJSCallback path (so the
        // JS method's returned JS Promise is awaited, not mis-read as the raw
        // value), and its `.promise` is the C++ `Promise<T>` the trampoline
        // chains on. A sync method stays the bare value/void type.
        let m = callback_method(true, ReturnKind::Value(NitroType::I32));
        assert_eq!(
            m.cxx_return_signature(),
            "std::shared_ptr<::ubrn::nitro::ForeignAsyncResult<int32_t>>"
        );

        let m = callback_method(true, ReturnKind::Void);
        assert_eq!(
            m.cxx_return_signature(),
            "std::shared_ptr<::ubrn::nitro::ForeignAsyncResult<void>>"
        );

        let m = callback_method(false, ReturnKind::Value(NitroType::I32));
        assert_eq!(m.cxx_return_signature(), "int32_t");

        let m = callback_method(false, ReturnKind::Void);
        assert_eq!(m.cxx_return_signature(), "void");
    }
}
