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

/// Drop every registered callback trait. Called at the end of a generate so a
/// later run in the same thread starts clean.
pub fn clear_callback_traits() {
    CALLBACK_TRAITS.with(|set| set.borrow_mut().clear());
}

fn is_registered_callback_trait(namespace: &str, name: &str) -> bool {
    CALLBACK_TRAITS.with(|set| {
        set.borrow()
            .contains(&(namespace.to_string(), name.to_string()))
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
                _ => {}
            }
        }

        Ok(Self {
            namespace: ns_name,
            crate_name,
            namespace_camel,
            functions,
            interfaces,
            callback_interfaces,
            records,
            enums,
            errors,
            rustbuffer_alloc: namespace.ffi_rustbuffer_alloc.0.clone(),
            rustbuffer_free: namespace.ffi_rustbuffer_free.0.clone(),
            rustbuffer_reserve: namespace.ffi_rustbuffer_reserve.0.clone(),
        })
    }

    pub fn namespace_api_ts_name(&self) -> String {
        format!("{}Api", self.namespace_camel)
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
    /// C++ exception class name — `<TsName>Error` to disambiguate from
    /// a same-named data enum, mirroring the codec class.
    // Reserved for the typed-error emission path (not yet wired into the
    // templates, which currently lift errors via `lift_fn`).
    #[allow(dead_code)]
    pub fn cxx_class(&self) -> String {
        format!("{}Error", self.ts_name)
    }

    /// Free-function decoder name in the owning namespace's
    /// `<namespace>_codecs.hpp`, qualified with the foreign namespace when
    /// the error is defined outside `current_ns` (so a cross-crate throws
    /// resolves against the included foreign codecs header).
    pub fn lift_fn(&self, current_ns: &str) -> String {
        let prefix = if self.namespace == current_ns {
            String::new()
        } else {
            format!("::margelo::nitro::{}::", self.namespace)
        };
        format!("{prefix}lift_{}Error", self.ts_name)
    }
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
        let ts_name = func.name.to_lower_camel_case();
        let cxx_name = sanitize_cxx_ident(&ts_name);
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
        })
    }

    fn from_constructor(ctor: &general::Constructor) -> Result<Self> {
        let ts_name = ctor.name.to_lower_camel_case();
        let cxx_name = sanitize_cxx_ident(&ts_name);
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
        let ts_name = factory_base.to_lower_camel_case();
        let cxx_name = ts_name.clone();
        let uniffi_symbol = ctor.callable.ffi_func.0.clone();
        let mut args = Vec::new();
        for arg in &ctor.inputs {
            args.push(NitroArg::from_general(arg)?);
        }
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
        })
    }

    fn from_method(method: &general::Method) -> Result<Self> {
        let ts_name = method.name.to_lower_camel_case();
        let cxx_name = sanitize_cxx_ident(&ts_name);
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

impl NitroFunction {
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
}

impl NitroInterface {
    fn from_general(iface: &general::Interface) -> Result<Self> {
        let ts_name = iface.name.to_upper_camel_case();
        let cxx_class = format!("Hybrid{}", ts_name);

        // The interface's own type — used as the return of every factory.
        // Resolving via `from_type` yields the correct namespace + name, so
        // the factory's `lift_expr` wraps the owned handle in the right
        // fully-qualified `Hybrid<Name>`.
        let self_ty = NitroType::from_type(&iface.self_type.ty)?;

        // One pass over the constructors: the first argless/sync/infallible
        // one becomes the `primary` (wired into the C++ default constructor,
        // kept in `constructors`); every other constructor becomes a factory
        // method on the namespace API (Nitro's argless `createHybridObject`
        // can't drive an argument-taking / async / fallible constructor).
        let mut constructors = Vec::new();
        let mut factories = Vec::new();
        let mut have_primary = false;
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
            let is_primary = !have_primary
                && parsed.args.is_empty()
                && !parsed.is_async
                && parsed.throws.is_none();
            if is_primary {
                have_primary = true;
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

        Ok(Self {
            ts_name,
            cxx_class,
            free_symbol: iface.ffi_func_free.0.clone(),
            clone_symbol: iface.ffi_func_clone.0.clone(),
            constructors,
            factories,
            methods,
        })
    }

    /// The interface's primary (argless, sync, infallible) constructor,
    /// if any. Nitro vends HybridObjects through
    /// `NitroModules.createHybridObject('<Name>')`, which runs the C++
    /// default constructor with no arguments — so we can only wire a
    /// uniffi constructor into that path when it takes no args. The
    /// common `#[uniffi::constructor] fn new() -> Arc<Self>` shape fits.
    /// Interfaces whose construction needs arguments stay default-handle
    /// (the namespace API can still hand them back from method returns).
    pub fn primary_constructor(&self) -> Option<&NitroFunction> {
        self.constructors
            .iter()
            .find(|c| c.args.is_empty() && !c.is_async && c.throws.is_none())
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
    /// async method returns `std::shared_ptr<Promise<T>>` — Nitro's contract
    /// for `Promise`-returning HybridObject methods, which the async
    /// trampoline awaits before driving uniffi's foreign-future callback.
    /// Mirror of [`NitroFunction::cxx_return_signature`].
    pub fn cxx_return_signature(&self) -> String {
        let inner = self.return_kind.cxx_type();
        if self.is_async {
            format!("std::shared_ptr<::margelo::nitro::Promise<{inner}>>")
        } else {
            inner
        }
    }
}

impl NitroCallbackInterface {
    /// Build the foreign-only shape from a UDL `callback interface`.
    fn from_general(cb: &general::CallbackInterface) -> Result<Self> {
        let ts_name = cb.name.to_upper_camel_case();
        let cxx_class = format!("Hybrid{}", ts_name);
        let vtable_init_symbol = cb.vtable.init_fn.0.clone();

        let mut methods = Vec::new();
        for method in &cb.methods {
            let res = (|| -> Result<NitroCallbackMethod> {
                let ts_name = method.name.to_lower_camel_case();
                let cxx_name = sanitize_cxx_ident(&ts_name);
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
                let ts_name = method.name.to_lower_camel_case();
                let cxx_name = sanitize_cxx_ident(&ts_name);
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
}

impl NitroArg {
    fn from_general(arg: &general::Argument) -> Result<Self> {
        Ok(Self {
            ts_name: sanitize_ts_arg_ident(&arg.name.to_lower_camel_case()),
            ty: NitroType::from_type(&arg.ty.ty)?,
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
    /// A placeholder for any uniffi type the Nitro backend can't model.
    /// `from_type` is total over uniffi's type universe today, so this is
    /// unreachable in practice; it remains as a defensive surface
    /// (`unknown` in TS, runtime-throwing codec thunks) so a future
    /// uniffi type addition degrades loudly rather than failing the whole
    /// module emission.
    Stub,
}

impl NitroType {
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
            // A uniffi `custom` type is a thin wrapper around a builtin
            // (e.g. `Url`/`String`, `JsonValue`/`String`) whose only role
            // on the FFI side is to inherit the builtin's wire format.
            // Recursing into the builtin gives us a working round-trip
            // immediately; renaming/converters live in the TS-side wrapper
            // module and don't affect the C ABI.
            Type::Custom { builtin, .. } => Self::from_type(builtin)?,
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
            Self::Optional(inner) => format!("({}) | null", inner.ts_type()),
            Self::Sequence(inner) => format!("({})[]", inner.ts_type()),
            Self::Map(k, v) => {
                // TS `Record<K, V>` requires `K` extend `string | number | symbol`.
                // bigint and complex keys fall back to `Map<K, V>`.
                if Self::is_record_key_compatible(k) {
                    format!("Record<{}, {}>", k.ts_type(), v.ts_type())
                } else {
                    format!("Map<{}, {}>", k.ts_type(), v.ts_type())
                }
            }
            Self::CallbackInterface { name, .. } => name.clone(),
            Self::Record { name, .. } => name.to_upper_camel_case(),
            Self::Enum { name, .. } => name.to_upper_camel_case(),
            Self::Interface { name, .. } => name.to_upper_camel_case(),
            Self::Stub => "unknown".into(),
        }
    }

    fn is_record_key_compatible(ty: &NitroType) -> bool {
        matches!(
            ty,
            Self::String
                | Self::U8
                | Self::U16
                | Self::U32
                | Self::I8
                | Self::I16
                | Self::I32
                | Self::F32
                | Self::F64
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
        }
    }

    /// True when this type crosses the FFI as a `RustBuffer`. Used by the
    /// callback trampolines: an arg lowered by Rust into a RustBuffer is
    /// handed to us by-value (Rust gives up ownership), so after lifting it
    /// the trampoline must free the buffer exactly once.
    pub fn is_rust_buffer(&self) -> bool {
        self.c_type() == "RustBuffer"
    }

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
                // impl that could be returned. The proxy ctor takes ownership
                // of the handle (uniffi already gave us our own reference). The
                // class lives in the callback's owning namespace, so qualify
                // when it's foreign.
                format!(
                    "std::make_shared<{prefix}Hybrid{cb}>(::ubrn::nitro::FromRustHandle{{{name}}})",
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
                "std::make_shared<::margelo::nitro::{}::Hybrid{}>({name})",
                namespace,
                type_name.to_upper_camel_case()
            ),
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
        matches!(self, Self::Bytes)
    }

    /// Zero-copy top-level lift that consumes the `RustBuffer` (see
    /// [`Self::lift_consumes_buffer`]). `free_symbol` is the namespace
    /// `ffi_<crate>_rustbuffer_free`, wired as the ArrayBuffer's finalizer.
    pub fn lift_owning_expr(&self, name: &str, current_ns: &str, free_symbol: &str) -> String {
        match self {
            Self::Bytes => {
                format!("ubrn::nitro::lift_bytes_owning<&{free_symbol}>({name})")
            }
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
            _ => Vec::new(),
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
    /// Records, enums, interfaces, callbacks and stubs are *not* surfaced:
    /// a record / data-enum reader exists but has no `to_string`, and — more
    /// importantly — an error enum used as another error's field has *no*
    /// reader emitted at all (errors become exception classes, not value
    /// types), so reading it would reference a non-existent `read_<Name>`.
    /// Such variants fall back to the tag-only message, exactly as before.
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
            Self::Record { .. }
            | Self::Enum { .. }
            | Self::Interface { .. }
            | Self::CallbackInterface { .. }
            | Self::Stub => false,
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
}

impl NitroRecord {
    fn from_general(record: &general::Record) -> Result<Self> {
        let ts_name = record.name.to_upper_camel_case();
        let fields = record
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| NitroRecordField::from_general(f, i))
            .collect();
        Ok(Self { ts_name, fields })
    }

    /// Headers this record's struct definition depends on (nested
    /// records / enums / interfaces), deduped and excluding its own.
    pub fn dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.ts_name);
        dedup_headers(
            self.fields.iter().flat_map(|f| f.ty.referenced_headers()),
            &own,
        )
    }
}

pub struct NitroRecordField {
    /// TS-facing field name (lowerCamelCase), matching what Nitrogen
    /// names the field on the auto-generated C++ struct.
    pub ts_name: String,
    /// Source Rust field name (snake_case) — kept for debug emission.
    #[allow(dead_code)]
    pub rust_name: String,
    pub ty: NitroType,
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
        Self {
            ts_name,
            rust_name: field.name.clone(),
            ty: NitroType::from_type_lossy(&field.ty.ty),
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
}

impl NitroEnum {
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
            flat: en.is_flat,
        })
    }

    /// Headers this enum's payload structs depend on, deduped and
    /// excluding its own (a recursive enum references itself, which is
    /// handled by in-file forward declaration, not an include).
    pub fn dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.ts_name);
        dedup_headers(
            self.variants
                .iter()
                .flat_map(|v| v.fields.iter())
                .flat_map(|f| f.ty.referenced_headers()),
            &own,
        )
    }
}

/// Make `name` safe to use as a C++ identifier by appending `_` when it
/// collides with a reserved keyword (C++20 keywords + alternative tokens +
/// `override`/`final` which are context-sensitive but reserved here because
/// the generated method bodies use them as virtual-override declarations).
///
/// Applied ONLY to the C++-facing identifier (`cxx_name`); the JS-facing
/// `ts_name` — and the string passed to `registerHybridMethod` — keep the
/// original spelling so the JS surface is unchanged. A uniffi method named
/// `delete` thus emits `Hybrid::delete_(...)` while staying registered as
/// `"delete"`.
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
    if CXX_KEYWORDS.contains(&name) {
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
    const TS_RESERVED_ARG_IDENTS: &[&str] = &[
        "this", "arguments", "eval", "default", "function", "in", "instanceof", "new", "return",
        "typeof", "void", "delete", "yield", "await",
    ];
    if TS_RESERVED_ARG_IDENTS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// Dedup + sort a header-name iterator, dropping `own` (a type never
/// includes its own header — recursion is handled by forward declaration).
fn dedup_headers(headers: impl Iterator<Item = String>, own: &str) -> Vec<String> {
    let mut set: BTreeSet<String> = headers.collect();
    set.remove(own);
    set.into_iter().collect()
}

pub struct NitroEnumVariant {
    /// UpperCamelCase variant name as it appears in the TS union literal.
    pub ts_name: String,
    /// lowerCamelCase discriminant tag string used on the JS side
    /// (`{ type: '<tag>' }`). Distinct from `ts_name` so the union member
    /// reads naturally in TS while the C++ enum member stays UpperCamel.
    pub tag: String,
    /// Associated-data fields, if any. Empty for unit variants.
    pub fields: Vec<NitroRecordField>,
}

impl NitroEnumVariant {
    /// Per-variant C++ payload struct name, e.g. `Tree_Node`. One struct
    /// per variant holds that variant's fields; the enum itself is a
    /// `std::variant` over these. Unit variants get an empty struct.
    pub fn cxx_struct_name(&self, enum_name: &str) -> String {
        format!("{}_{}", enum_name.to_upper_camel_case(), self.ts_name)
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
            tag: variant.name.to_lower_camel_case(),
            fields: variant
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| NitroRecordField::from_general(f, i))
                .collect(),
        }
    }
}

/// A uniffi error enum (UDL `[Error]` or Rust `#[derive(Error)]`).
/// Codegen for errors is a future scope — they become C++ exception
/// classes plus `lift_<Name>Error` decoders that map the variant
/// ordinal back to the typed exception. For now we capture enough
/// metadata (name + variants + flat flag) so templates can iterate
/// `module.errors` without panicking, but no codec body is emitted.
pub struct NitroError {
    pub ts_name: String,
    pub variants: Vec<NitroEnumVariant>,
    pub flat: bool,
}

impl NitroError {
    /// C++ exception class name we emit (matches `NitroErrorRef::cxx_class`).
    pub fn cxx_class(&self) -> String {
        format!("{}Error", self.ts_name)
    }
    /// Lifter free-function name (matches `NitroErrorRef::lift_fn`).
    pub fn lift_fn(&self) -> String {
        format!("lift_{}Error", self.ts_name)
    }
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
            flat: en.is_flat,
        })
    }
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

    fn callback_method(is_async: bool, return_kind: ReturnKind) -> NitroCallbackMethod {
        NitroCallbackMethod {
            ts_name: "onThing".into(),
            cxx_name: "onThing".into(),
            args: Vec::new(),
            return_kind,
            is_async,
            uniffi_symbol: None,
            throws: None,
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
    fn async_callback_method_cxx_return_wraps_in_promise() {
        // The C++ HybridObject method for an async foreign-trait method must
        // return `std::shared_ptr<Promise<T>>` so the trampoline can await it;
        // a sync method stays the bare value/void type.
        let m = callback_method(true, ReturnKind::Value(NitroType::I32));
        assert_eq!(
            m.cxx_return_signature(),
            "std::shared_ptr<::margelo::nitro::Promise<int32_t>>"
        );

        let m = callback_method(true, ReturnKind::Void);
        assert_eq!(
            m.cxx_return_signature(),
            "std::shared_ptr<::margelo::nitro::Promise<void>>"
        );

        let m = callback_method(false, ReturnKind::Value(NitroType::I32));
        assert_eq!(m.cxx_return_signature(), "int32_t");

        let m = callback_method(false, ReturnKind::Void);
        assert_eq!(m.cxx_return_signature(), "void");
    }
}
