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
    Ok(())
}

/// Emit the per-enum `<Name>.hpp` (flat `enum class` or tagged
/// `std::variant` representation + `JSIConverter`).
pub(super) fn write_enum(cpp_dir: &Utf8Path, module: &NitroModule, en: &NitroEnum) -> Result<()> {
    let text = EnumHpp { module, en }.render()?;
    let path = cpp_dir.join(format!("{}.hpp", en.ts_name));
    ubrn_common::write_file(path, text)?;
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
#[template(syntax = "cpp", escape = "none", path = "enum.hpp")]
struct EnumHpp<'a> {
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
