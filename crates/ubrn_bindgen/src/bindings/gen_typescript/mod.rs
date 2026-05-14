/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
pub(crate) mod api_module;
mod config;
pub(crate) mod ffi_module;
pub(crate) mod ffi_module_player;
mod type_mapping;
mod util;

use anyhow::{Context, Result};
use askama::Template;

use self::api_module::{TsTypeDefinition, TsUniffiTrait};
use self::ffi_module::FfiDefinitionDecl;
use self::ffi_module_player::LibResolution;
pub(crate) use self::{config::TsConfig as Config, util::format_directory};
use super::metadata::ModuleMetadata;
use crate::switches::AbiFlavor;

pub(crate) fn generate_lowlevel_code(ffi_module: ffi_module::TsFfiModule) -> Result<String> {
    LowlevelTsWrapper::new(ffi_module)
        .render()
        .context("generating lowlevel typescript from IR failed")
}

pub(crate) fn generate_player_lowlevel_code(
    player_module: ffi_module_player::PlayerFfiModule,
) -> Result<String> {
    PlayerLowlevelTsWrapper::new(player_module)
        .render()
        .context("generating player lowlevel typescript from IR failed")
}

pub(crate) fn generate_index_code(
    modules: Vec<ModuleMetadata>,
    flavor: AbiFlavor,
    import_extension: Option<&str>,
) -> Result<String> {
    // Pre-compute the per-module relative import specifier here (in IR-build
    // territory) rather than branching inside `index.ts`. Keeps the template
    // free of `{% if has_ext %}` clutter and gives us a single place that
    // applies the suffix consistently across the file.
    let cfg = config::TsConfig::default().with_import_extension(import_extension);
    let module_specifiers = modules
        .iter()
        .map(|m| cfg.import_specifier(&m.ts()))
        .collect();
    IndexTsWrapper {
        modules,
        flavor,
        module_specifiers,
    }
    .render()
    .context("generating index.ts from IR failed")
}

pub(crate) fn generate_api_code_from_ir(api_module: api_module::TsApiModule) -> Result<String> {
    TsApiWrapperV2::new(api_module)
        .render()
        .context("generating wrapper typescript from IR failed")
}

#[derive(Template)]
#[template(syntax = "ts", escape = "none", path = "wrapper.ts")]
struct TsApiWrapperV2 {
    module: api_module::TsApiModule,
}

impl TsApiWrapperV2 {
    fn new(module: api_module::TsApiModule) -> Self {
        Self { module }
    }
}

#[derive(Template)]
#[template(syntax = "ts", escape = "none", path = "wrapper-ffi.ts")]
struct LowlevelTsWrapper {
    module: ffi_module::TsFfiModule,
}

impl LowlevelTsWrapper {
    fn new(module: ffi_module::TsFfiModule) -> Self {
        Self { module }
    }
}

#[derive(Template)]
#[template(syntax = "ts", escape = "none", path = "wrapper-ffi-player.ts")]
struct PlayerLowlevelTsWrapper {
    module: ffi_module_player::PlayerFfiModule,
}

impl PlayerLowlevelTsWrapper {
    fn new(module: ffi_module_player::PlayerFfiModule) -> Self {
        Self { module }
    }
}

#[derive(Template)]
#[template(syntax = "ts", escape = "none", path = "index.ts")]
struct IndexTsWrapper {
    modules: Vec<ModuleMetadata>,
    flavor: AbiFlavor,
    /// Pre-computed relative import specifier per module (matches the order
    /// of `modules`). See `generate_index_code` for the construction site.
    module_specifiers: Vec<String>,
}

#[cfg(test)]
mod index_template_tests {
    use super::*;

    fn render(import_extension: Option<&str>) -> String {
        generate_index_code(
            vec![ModuleMetadata::new("arithmetic")],
            AbiFlavor::Napi,
            import_extension,
        )
        .expect("render")
    }

    #[test]
    fn index_default_is_extensionless() {
        let out = render(None);
        // Both the re-export and the namespaced import must point at the
        // bare specifier — preserves byte-for-byte compatibility with the
        // current react-native/bundler workflow.
        assert!(
            out.contains("export * from './arithmetic';"),
            "missing extensionless re-export, got:\n{out}"
        );
        assert!(
            out.contains("import * as arithmetic from './arithmetic';"),
            "missing extensionless namespace import, got:\n{out}"
        );
        assert!(
            !out.contains("./arithmetic.js"),
            "default mode should not emit .js suffix, got:\n{out}"
        );
    }

    #[test]
    fn index_with_js_extension_suffixes_specifiers() {
        let out = render(Some("js"));
        assert!(
            out.contains("export * from './arithmetic.js';"),
            "missing .js-suffixed re-export, got:\n{out}"
        );
        assert!(
            out.contains("import * as arithmetic from './arithmetic.js';"),
            "missing .js-suffixed namespace import, got:\n{out}"
        );
    }

    #[test]
    fn index_with_leading_dot_extension_is_normalized() {
        // `.js` and `js` are equivalent — operators write either.
        let out = render(Some(".js"));
        assert!(
            out.contains("./arithmetic.js"),
            "leading-dot ext should normalize to `.js`, got:\n{out}"
        );
        assert!(
            !out.contains("./arithmetic..js"),
            "leading-dot ext must not double up, got:\n{out}"
        );
    }
}
