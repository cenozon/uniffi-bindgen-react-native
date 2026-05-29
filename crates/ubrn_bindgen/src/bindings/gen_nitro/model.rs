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

use anyhow::Result;
use heck::{ToLowerCamelCase, ToUpperCamelCase};

use uniffi_bindgen::pipeline::general;

use super::HybridObjectEntry;

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
                    interfaces.push(NitroInterface::from_general(iface)?);
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

    /// Per-type headers the namespace API's method declarations reference.
    /// Deduped + sorted so the emitted `#include` block is stable.
    pub fn api_dependency_headers(&self) -> Vec<String> {
        dedup_headers(
            self.functions.iter().flat_map(|f| f.referenced_headers()),
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

    /// Free-function decoder name in `<namespace>_codecs.hpp`.
    pub fn lift_fn(&self) -> String {
        format!("lift_{}Error", self.ts_name)
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
        let cxx_name = ts_name.clone();
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
        let cxx_name = ts_name.clone();
        let uniffi_symbol = ctor.callable.ffi_func.0.clone();
        let mut args = Vec::new();
        for arg in &ctor.inputs {
            args.push(NitroArg::from_general(arg)?);
        }
        // Constructors return a handle to the new object. Surface as void
        // for now (V1 surface skips interface construction); follow-up
        // wires this through to the HybridObject factory.
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

    fn from_method(method: &general::Method) -> Result<Self> {
        let ts_name = method.name.to_lower_camel_case();
        let cxx_name = ts_name.clone();
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
        general::Type::Enum { name, .. } => Some(NitroErrorRef {
            ts_name: name.to_upper_camel_case(),
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
}

pub struct NitroInterface {
    pub ts_name: String,
    pub cxx_class: String,
    /// `uniffi_<crate>_fn_free_<obj>` — comes straight from the pipeline.
    pub free_symbol: String,
    /// `uniffi_<crate>_fn_clone_<obj>` — for now unused; will be needed
    /// when methods take other interface references as args.
    #[allow(dead_code)]
    pub clone_symbol: String,
    /// Parsed from uniffi metadata. The argless primary constructor (if
    /// any) is wired into the C++ default constructor so
    /// `NitroModules.createHybridObject('<Name>')` yields a live Rust
    /// object; see [`Self::primary_constructor`].
    pub constructors: Vec<NitroFunction>,
    pub methods: Vec<NitroFunction>,
}

impl NitroInterface {
    fn from_general(iface: &general::Interface) -> Result<Self> {
        let ts_name = iface.name.to_upper_camel_case();
        let cxx_class = format!("Hybrid{}", ts_name);

        let mut constructors = Vec::new();
        for ctor in &iface.constructors {
            match NitroFunction::from_constructor(ctor) {
                Ok(f) => constructors.push(f),
                Err(e) => eprintln!(
                    "nitro: skipping constructor `{}.{}`: {e}",
                    iface.name, ctor.name
                ),
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

    /// Per-type headers this interface's method declarations reference,
    /// excluding its own (an interface method that returns / takes the
    /// same interface resolves via this class's own declaration).
    pub fn dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.cxx_class);
        dedup_headers(
            self.methods.iter().flat_map(|m| m.referenced_headers()),
            &own,
        )
    }
}

/// A uniffi `callback interface`. JS-side implements the methods; the
/// generated C++ trampoline file (`HybridFooCallback.{hpp,cpp}`)
/// registers a vtable with Rust on first use so Rust can dispatch into
/// the JS impl.
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
}

pub struct NitroCallbackMethod {
    pub ts_name: String,
    pub cxx_name: String,
    pub args: Vec<NitroArg>,
    pub return_kind: ReturnKind,
}

impl NitroCallbackInterface {
    fn from_general(cb: &general::CallbackInterface) -> Result<Self> {
        let ts_name = cb.name.to_upper_camel_case();
        let cxx_class = format!("Hybrid{}", ts_name);
        let vtable_init_symbol = cb.vtable.init_fn.0.clone();

        let mut methods = Vec::new();
        for method in &cb.methods {
            let res = (|| -> Result<NitroCallbackMethod> {
                let ts_name = method.name.to_lower_camel_case();
                let cxx_name = ts_name.clone();
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
        })
    }
}

pub struct NitroArg {
    pub ts_name: String,
    pub ty: NitroType,
}

impl NitroArg {
    fn from_general(arg: &general::Argument) -> Result<Self> {
        Ok(Self {
            ts_name: arg.name.to_lower_camel_case(),
            ty: NitroType::from_type(&arg.ty.ty)?,
        })
    }

    /// Convenience for callback trampoline templates: lift the arg
    /// from its `<ts_name>_lowered` (C-ABI) form to the C++ value.
    /// Used inside the per-callback-method `extern "C"` trampoline,
    /// which receives the Rust-lowered shape and needs to call into
    /// the foreign HybridObject method with C++ types.
    pub fn lifted_from_lowered_expr(&self) -> String {
        let lowered_name = format!("{}_lowered", self.ts_name);
        self.ty.lift_expr(&lowered_name)
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
    /// surface: `std::chrono::nanoseconds`.
    Duration,
    Optional(Box<NitroType>),
    Sequence(Box<NitroType>),
    Map(Box<NitroType>, Box<NitroType>),
    /// A foreign-implemented callback. The C++ side accepts a
    /// `std::shared_ptr<Hybrid<Name>Spec>` and registers it with the
    /// vtable handle map before passing the handle to Rust.
    CallbackInterface(String),
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
            Type::CallbackInterface { name, .. } => {
                Self::CallbackInterface(name.to_upper_camel_case())
            }
            Type::Record { namespace, name } => Self::Record {
                namespace: namespace.clone(),
                name: name.clone(),
            },
            Type::Enum { namespace, name } => Self::Enum {
                namespace: namespace.clone(),
                name: name.clone(),
            },
            Type::Interface {
                namespace,
                name,
                imp,
            } => match imp {
                general::ObjectImpl::CallbackTrait => {
                    Self::CallbackInterface(name.to_upper_camel_case())
                }
                _ => Self::Interface {
                    namespace: namespace.clone(),
                    name: name.clone(),
                },
            },
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
            Self::CallbackInterface(name) => name.clone(),
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
            Self::CallbackInterface(name) => {
                format!("std::shared_ptr<Hybrid{}>", name)
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
            Self::CallbackInterface(_) | Self::Interface { .. } => "uint64_t",
        }
    }

    /// Expression that lowers a `cxx_type`-typed value named `<name>` to
    /// the `c_type` for an FFI call. `alloc_symbol` / `reserve_symbol`
    /// are the namespace's `ffi_<crate>_rustbuffer_alloc` /
    /// `ffi_<crate>_rustbuffer_reserve` symbols — both needed by the
    /// `RustBufferWriter` template.
    pub fn lower_expr(&self, name: &str, alloc_symbol: &str, reserve_symbol: &str) -> String {
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
                let inner_writer = inner.write_fn_template_arg(alloc_symbol, reserve_symbol);
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
                let inner_writer = inner.write_fn_template_arg(alloc_symbol, reserve_symbol);
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
                let k_writer = k.write_fn_template_arg(alloc_symbol, reserve_symbol);
                let v_writer = v.write_fn_template_arg(alloc_symbol, reserve_symbol);
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
            Self::CallbackInterface(cb_name) => {
                // Register the JS-side instance with the handle map.
                // The trampoline file ensures vtable init has happened
                // by the time this expression runs.
                format!(
                    "ubrn::nitro::CallbackHandleMap<Hybrid{cb}>::instance().insert({name})",
                    cb = cb_name,
                )
            }
            // Records + enums delegate to the free functions in
            // `<namespace>_codecs.hpp`; they're unqualified because the
            // generated impl file is inside that same namespace.
            Self::Record {
                name: type_name, ..
            }
            | Self::Enum {
                name: type_name, ..
            } => format!("lower_{}({name})", type_name.to_upper_camel_case()),
            Self::Interface { .. } => format!("{name}->raw_handle()"),
            Self::Stub => {
                format!("::ubrn::nitro::lower_stub(/* unsupported in nitro v1 */ {name})")
            }
            _ => format!("ubrn::nitro::lower_{}({name})", self.lower_suffix()),
        }
    }

    /// Expression that lifts a `c_type`-typed value to the `cxx_type`.
    /// For RustBuffer-bearing types the caller is responsible for
    /// freeing the buffer after the lift.
    pub fn lift_expr(&self, name: &str) -> String {
        match self {
            Self::Bool => format!("ubrn::nitro::lift_bool({name})"),
            Self::String => format!("ubrn::nitro::lift_string({name})"),
            Self::Bytes => format!("ubrn::nitro::lift_bytes({name})"),
            Self::Timestamp => format!("ubrn::nitro::lift_timestamp({name})"),
            Self::Duration => format!("ubrn::nitro::lift_duration({name})"),
            Self::Optional(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_reader = inner.read_fn_template_arg();
                format!(
                    "ubrn::nitro::lift_optional<{ty}, {reader}>({name})",
                    ty = inner_cxx,
                    reader = inner_reader,
                )
            }
            Self::Sequence(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_reader = inner.read_fn_template_arg();
                format!(
                    "ubrn::nitro::lift_sequence<{ty}, {reader}>({name})",
                    ty = inner_cxx,
                    reader = inner_reader,
                )
            }
            Self::Map(k, v) => {
                let k_cxx = k.cxx_type();
                let v_cxx = v.cxx_type();
                let k_reader = k.read_fn_template_arg();
                let v_reader = v.read_fn_template_arg();
                format!(
                    "ubrn::nitro::lift_map<{kt}, {vt}, {kr}, {vr}>({name})",
                    kt = k_cxx,
                    vt = v_cxx,
                    kr = k_reader,
                    vr = v_reader,
                )
            }
            Self::CallbackInterface(_) => {
                // Callback interfaces returned from Rust would mean Rust
                // owns a Box<dyn Trait> — not yet supported in the
                // Nitro backend (would need a Rust-side trampoline
                // HybridObject impl).
                format!(
                    "throw std::runtime_error(\"Nitro: lifting CallbackInterface from Rust not yet supported (got handle {name})\")"
                )
            }
            // Records + enums delegate to the free functions in
            // `<namespace>_codecs.hpp`.
            Self::Record {
                name: type_name, ..
            }
            | Self::Enum {
                name: type_name, ..
            } => format!("lift_{}({name})", type_name.to_upper_camel_case()),
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

    /// Template argument used as the per-element `write_` thunk when
    /// this type appears inside a composite. Composites nest via
    /// function pointers, so the inner write/read are spelled as
    /// template arg expressions, not function calls.
    fn write_fn_template_arg(&self, alloc_symbol: &str, reserve_symbol: &str) -> String {
        match self {
            Self::Bool => format!("&ubrn::nitro::write_bool<&{alloc_symbol}, &{reserve_symbol}>"),
            Self::String => {
                format!("&ubrn::nitro::write_string<&{alloc_symbol}, &{reserve_symbol}>")
            }
            Self::Optional(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_writer = inner.write_fn_template_arg(alloc_symbol, reserve_symbol);
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
                let inner_writer = inner.write_fn_template_arg(alloc_symbol, reserve_symbol);
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
                let k_writer = k.write_fn_template_arg(alloc_symbol, reserve_symbol);
                let v_writer = v.write_fn_template_arg(alloc_symbol, reserve_symbol);
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
            Self::CallbackInterface(_) => {
                // Composites containing callback interfaces aren't a
                // uniffi shape that appears in practice — surface as
                // unimplemented if it ever does.
                "&ubrn::nitro::unsupported_callback_inside_composite".into()
            }
            // Nested record / enum inside a composite delegate to the
            // per-type `write_<Name>` stream thunk emitted in
            // `<namespace>_codecs.hpp`. Those have the exact
            // `(RustBufferWriter<Alloc, Reserve>&, const T&)` signature the
            // composite thunks expect, so they nest as function-pointer
            // template args without any wrapper.
            Self::Record {
                name: type_name, ..
            }
            | Self::Enum {
                name: type_name, ..
            } => format!("&write_{}", type_name.to_upper_camel_case()),
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

    fn read_fn_template_arg(&self) -> String {
        match self {
            Self::Bool => "&ubrn::nitro::read_bool".into(),
            Self::String => "&ubrn::nitro::read_string".into(),
            Self::Bytes => "&ubrn::nitro::read_bytes".into(),
            Self::Timestamp => "&ubrn::nitro::read_timestamp".into(),
            Self::Duration => "&ubrn::nitro::read_duration".into(),
            Self::Optional(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_reader = inner.read_fn_template_arg();
                format!(
                    "&ubrn::nitro::read_optional<{ty}, {reader}>",
                    ty = inner_cxx,
                    reader = inner_reader,
                )
            }
            Self::Sequence(inner) => {
                let inner_cxx = inner.cxx_type();
                let inner_reader = inner.read_fn_template_arg();
                format!(
                    "&ubrn::nitro::read_sequence<{ty}, {reader}>",
                    ty = inner_cxx,
                    reader = inner_reader,
                )
            }
            Self::Map(k, v) => {
                let k_cxx = k.cxx_type();
                let v_cxx = v.cxx_type();
                let k_reader = k.read_fn_template_arg();
                let v_reader = v.read_fn_template_arg();
                format!(
                    "&ubrn::nitro::read_map<{kt}, {vt}, {kr}, {vr}>",
                    kt = k_cxx,
                    vt = v_cxx,
                    kr = k_reader,
                    vr = v_reader,
                )
            }
            Self::CallbackInterface(_) => {
                "&ubrn::nitro::unsupported_callback_inside_composite".into()
            }
            Self::Record {
                name: type_name, ..
            }
            | Self::Enum {
                name: type_name, ..
            } => format!("&read_{}", type_name.to_upper_camel_case()),
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
            Self::Interface { name, .. } => {
                vec![format!("Hybrid{}.hpp", name.to_upper_camel_case())]
            }
            _ => Vec::new(),
        }
    }

    /// Statement that serializes `<base>.<field>` into the open
    /// `RustBufferWriter` named `w`, field-by-field, inside a record /
    /// enum stream codec. Every type — primitive, composite, nested
    /// record/enum, interface — funnels through the same `(writer, value)`
    /// thunk shape, so the record codec body is a uniform field walk.
    pub fn stream_write_stmt(
        &self,
        base: &str,
        field: &str,
        alloc_symbol: &str,
        reserve_symbol: &str,
    ) -> String {
        // The composite / nested thunks are spelled as function-pointer
        // template args (`&fn<...>`); strip the leading `&` to call them.
        let thunk = self.write_fn_template_arg(alloc_symbol, reserve_symbol);
        let callee = thunk.strip_prefix('&').unwrap_or(&thunk);
        format!("{callee}(w, {base}.{field});")
    }

    /// Expression that deserializes one field of this type from the open
    /// `RustBufferReader` named `r`. Mirror of [`Self::stream_write_stmt`].
    pub fn stream_read_expr(&self) -> String {
        let thunk = self.read_fn_template_arg();
        let callee = thunk.strip_prefix('&').unwrap_or(&thunk);
        format!("{callee}(r)")
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
            .map(NitroRecordField::from_general)
            .collect();
        Ok(Self { ts_name, fields })
    }

    /// Headers this record's struct definition depends on (nested
    /// records / enums / interfaces), deduped and excluding its own.
    pub fn dependency_headers(&self) -> Vec<String> {
        let own = format!("{}.hpp", self.ts_name);
        dedup_headers(self.fields.iter().flat_map(|f| f.ty.referenced_headers()), &own)
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
    fn from_general(field: &general::Field) -> Self {
        Self {
            ts_name: field.name.to_lower_camel_case(),
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

/// Dedup + sort a header-name iterator, dropping `own` (a type never
/// includes its own header — recursion is handled by forward declaration).
fn dedup_headers(headers: impl Iterator<Item = String>, own: &str) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = headers.collect();
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
}

impl NitroEnumVariant {
    fn from_general(variant: &general::Variant) -> Self {
        Self {
            ts_name: variant.name.to_upper_camel_case(),
            tag: variant.name.to_lower_camel_case(),
            fields: variant
                .fields
                .iter()
                .map(NitroRecordField::from_general)
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
