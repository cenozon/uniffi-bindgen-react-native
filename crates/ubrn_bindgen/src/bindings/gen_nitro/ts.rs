/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
//! TypeScript emission for the Nitro backend. Two files per namespace:
//!
//! 1. `<Namespace>.nitro.ts` — the Nitrogen-consumable spec. Declares
//!    every HybridObject interface (the namespace API + each uniffi
//!    `Object`-derived interface). Nitrogen reads this to generate the
//!    `HybridXxxSpec` base classes that our C++ impls extend.
//!
//! 2. `<namespace>.ts` — the consumer-facing TS module. Re-exports the
//!    HybridObject types and exposes a singleton instance of the
//!    namespace API HybridObject (`createHybridObject` returns a cached
//!    one) so user code can call `nsApi.add(2n, 3n)` directly.

use anyhow::Result;
use askama::Template;
use camino::Utf8Path;

use super::model::NitroModule;

/// Write `<Namespace>.nitro.ts` next to the other bindings files.
pub(super) fn write_spec(ts_dir: &Utf8Path, module: &NitroModule) -> Result<()> {
    let path = ts_dir.join(module.nitro_ts_filename());
    let text = NitroSpec { module }.render()?;
    ubrn_common::write_file(path, text)?;
    Ok(())
}

/// Write `<namespace>.ts` — the user-facing module that imports types
/// from the spec and instantiates the namespace API HybridObject.
pub(super) fn write_reexport_module(
    ts_dir: &Utf8Path,
    module: &NitroModule,
    _module_meta: &crate::bindings::metadata::ModuleMetadata,
) -> Result<()> {
    let path = ts_dir.join(module.reexport_ts_filename());
    let text = NitroReexport { module }.render()?;
    ubrn_common::write_file(path, text)?;
    Ok(())
}

#[derive(Template)]
#[template(syntax = "ts", escape = "none", path = "namespace.nitro.ts")]
struct NitroSpec<'a> {
    module: &'a NitroModule,
}

#[derive(Template)]
#[template(syntax = "ts", escape = "none", path = "namespace.ts")]
struct NitroReexport<'a> {
    module: &'a NitroModule,
}
