/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct TsConfig {
    #[serde(default)]
    pub(crate) log_level: LogLevel,
    #[serde(default)]
    pub(crate) console_import: Option<String>,
    #[serde(default)]
    pub(crate) custom_types: HashMap<String, CustomTypeConfig>,
    #[serde(default)]
    pub(crate) strict_object_types: bool,
    /// When `true`, omit `// @ts-nocheck` from generated files so that
    /// `tsc` reports type errors. Defaults to `false` (generated files
    /// include `@ts-nocheck` to avoid noise in downstream projects).
    #[serde(default)]
    pub(crate) strict_type_checking: bool,
    /// When `true`, emit byte arrays (`Vec<u8>`) as `Uint8Array` instead of `ArrayBuffer`.
    #[serde(default)]
    pub(crate) strict_byte_arrays: bool,
    /// Optional extension to append to relative import specifiers in generated
    /// TypeScript (e.g. `"js"` produces `from './foo.js'`). Empty/unset =
    /// today's behavior (`from './foo'`).
    ///
    /// `tsc` preserves import specifiers verbatim into the emitted `.js`; Node
    /// ESM resolution rejects extensionless specifiers (`ERR_MODULE_NOT_FOUND`).
    /// Bundlers (metro, webpack, rollup, esbuild) accept either form, so leaving
    /// this unset keeps existing react-native and bundler-fed consumers
    /// byte-identical. Set to `"js"` for `tsc --module nodenext` consumers.
    ///
    /// A leading `.` is tolerated and stripped (`".js"` and `"js"` are
    /// equivalent).
    #[serde(default)]
    pub(crate) import_extension: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LogLevel {
    #[default]
    None,
    Debug,
    Verbose,
}

impl LogLevel {
    pub(crate) fn is_verbose(&self) -> bool {
        matches!(self, Self::Verbose)
    }
    pub(crate) fn is_debug(&self) -> bool {
        matches!(self, Self::Debug | Self::Verbose)
    }
}

impl TsConfig {
    pub(crate) fn is_verbose(&self) -> bool {
        self.log_level.is_verbose()
    }
    pub(crate) fn is_debug(&self) -> bool {
        self.log_level.is_debug()
    }

    /// Returns the normalized extension (without leading dot) if any, else `None`.
    pub(crate) fn import_extension(&self) -> Option<&str> {
        self.import_extension
            .as_deref()
            .map(|s| s.trim_start_matches('.'))
            .filter(|s| !s.is_empty())
    }

    /// Build a relative import specifier of the form `./{name}` (default) or
    /// `./{name}.{ext}` when `import_extension` is set. Use this everywhere
    /// the generator emits a relative path to a sibling generated module so
    /// the suffix is applied uniformly.
    pub(crate) fn import_specifier(&self, name: &str) -> String {
        match self.import_extension() {
            Some(ext) => format!("./{name}.{ext}"),
            None => format!("./{name}"),
        }
    }

    /// Apply a CLI override for `import_extension`. The CLI flag is the
    /// more-explicit signal so it takes precedence over the per-crate
    /// `uniffi.toml` value when set; when the CLI flag is unset we leave
    /// the existing config value (including `None`) alone.
    pub(crate) fn with_import_extension(mut self, ext: Option<&str>) -> Self {
        if let Some(ext) = ext {
            self.import_extension = Some(ext.to_string());
        }
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct CustomTypeConfig {
    #[serde(default)]
    pub(crate) imports: Vec<(String, String)>,
    pub(crate) type_name: Option<String>,
    #[serde(alias = "lift")]
    pub(crate) into_custom: String,
    #[serde(alias = "lower")]
    pub(crate) from_custom: String,
}

impl CustomTypeConfig {
    pub(crate) fn lift(&self, variable: &str) -> String {
        self.into_custom.replace("{}", variable)
    }
    pub(crate) fn lower(&self, variable: &str) -> String {
        self.from_custom.replace("{}", variable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(ext: Option<&str>) -> TsConfig {
        TsConfig {
            import_extension: ext.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn import_specifier_default_is_extensionless() {
        let c = cfg(None);
        assert_eq!(c.import_specifier("arithmetic"), "./arithmetic");
        assert_eq!(c.import_specifier("foo-ffi"), "./foo-ffi");
    }

    #[test]
    fn import_specifier_appends_js() {
        let c = cfg(Some("js"));
        assert_eq!(c.import_specifier("arithmetic"), "./arithmetic.js");
        assert_eq!(c.import_specifier("foo-ffi"), "./foo-ffi.js");
    }

    #[test]
    fn import_specifier_strips_leading_dot() {
        // `.js` and `js` should be equivalent — users are likely to write either.
        let c = cfg(Some(".js"));
        assert_eq!(c.import_specifier("arithmetic"), "./arithmetic.js");
    }

    #[test]
    fn import_specifier_treats_empty_string_as_unset() {
        let c = cfg(Some(""));
        assert_eq!(c.import_specifier("arithmetic"), "./arithmetic");
        assert!(c.import_extension().is_none());
    }

    #[test]
    fn import_specifier_supports_arbitrary_extension() {
        // Forward-compat: future generators may emit `.mjs`/`.cjs`.
        let c = cfg(Some("mjs"));
        assert_eq!(c.import_specifier("arithmetic"), "./arithmetic.mjs");
    }
}
