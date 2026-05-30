/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
use std::{cell::OnceCell, collections::BTreeMap, rc::Rc};

use anyhow::Result;
use askama::DynTemplate;
use camino::{Utf8Path, Utf8PathBuf};
use path_slash::PathExt;

use ubrn_bindgen::{HybridObjectEntry, ModuleMetadata};
use ubrn_common::{mk_dir, CrateMetadata};

use crate::config::ProjectConfig;

pub(crate) trait RenderedFile: DynTemplate {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf;
    fn relative_to(&self, project_root: &Utf8Path, to: &Utf8Path) -> Utf8PathBuf {
        let file = self.path(project_root);
        let from = file
            .parent()
            .expect("Expected this file to have a directory");
        let rel =
            pathdiff::diff_utf8_paths(to, from).expect("Should be able to find a relative path");
        // Normalize to forward slashes so templates produce valid paths on Windows.
        Utf8PathBuf::from(rel.as_std_path().to_slash_lossy().as_ref())
    }
    fn filter_by(&self) -> bool {
        true
    }
    /// Optional hook to transform the text after rendered from the Askama template.
    fn transform_str(&self, _project_root: &Utf8Path, contents: String) -> Result<String> {
        Ok(contents)
    }
}

pub(crate) struct TemplateConfig {
    pub(crate) project: ProjectConfig,
    pub(crate) rust_crate: CrateMetadata,
    pub(crate) modules: Vec<ModuleMetadata>,
    /// HybridObjects emitted by `ubrn_bindgen`'s `gen_nitro` backend.
    /// Populated only for the Nitro flavor; empty for JSI / Napi / WASM.
    /// The Nitro platform-glue templates (`nitro.json`,
    /// `nitro-CMakeLists.txt`) iterate this list to produce the
    /// autolinking entries + the C++ source manifest. Sorted by name in
    /// [`TemplateConfig::new`] so re-running emission yields byte-
    /// identical output regardless of `gen_nitro`'s iteration order.
    pub(crate) nitro_hybrid_objects: Vec<HybridObjectEntry>,
    pub(crate) uses_kotlin: OnceCell<bool>,
    pub(crate) native_bindings: bool,
}

impl TemplateConfig {
    pub(crate) fn new(
        project: ProjectConfig,
        rust_crate: CrateMetadata,
        modules: Vec<ModuleMetadata>,
        nitro_hybrid_objects: Vec<HybridObjectEntry>,
        native_bindings: bool,
    ) -> Self {
        let mut modules = modules;
        modules.sort_by_key(|m| m.ts());
        // Sort by TS name to keep the emitted nitro.json + CMakeLists.txt
        // deterministic across runs (gen_nitro already de-duplicates via
        // BTreeSet, but a HashSet anywhere upstream or future
        // rearrangement would silently desynchronize the byte-identical
        // contract).
        let mut nitro_hybrid_objects = nitro_hybrid_objects;
        nitro_hybrid_objects.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            project,
            rust_crate,
            modules,
            nitro_hybrid_objects,
            native_bindings,
            uses_kotlin: OnceCell::new(),
        }
    }
}

pub(crate) fn get_template_config(
    project: ProjectConfig,
    rust_crate: CrateMetadata,
    modules: Vec<ModuleMetadata>,
    nitro_hybrid_objects: Vec<HybridObjectEntry>,
    native_bindings: bool,
) -> Rc<TemplateConfig> {
    Rc::new(TemplateConfig::new(
        project,
        rust_crate,
        modules,
        nitro_hybrid_objects,
        native_bindings,
    ))
}

pub(crate) fn render_files(
    config: Rc<TemplateConfig>,
    files: impl Iterator<Item = Rc<dyn RenderedFile>>,
) -> Result<()> {
    let files = files.filter(|f| f.filter_by());
    let project_root = config.project.project_root();
    let map = render_templates(project_root, files)?;
    let exclude_files = config.project.exclude_files();
    for (path, contents) in map {
        // We don't want to write files that the config file has excluded.
        // In order to test if it is excluded, we need to get the file path
        // relative to the project_root.
        let rel = pathdiff::diff_utf8_paths(&path, project_root)
            .expect("path should be relative to root");
        if exclude_files.is_match(&rel) {
            continue;
        }
        let parent = path.parent().expect("Parent for path");
        mk_dir(parent)?;
        ubrn_common::write_file(path, contents)?;
    }

    Ok(())
}

fn render_templates(
    project_root: &Utf8Path,
    files: impl Iterator<Item = Rc<dyn RenderedFile>>,
) -> Result<BTreeMap<Utf8PathBuf, String>> {
    let mut map = BTreeMap::default();
    for f in files {
        let text = f.dyn_render()?;
        let path = f.path(project_root);
        map.insert(path, f.transform_str(project_root, text)?);
    }
    Ok(map)
}

#[macro_export]
macro_rules! templated_file {
    ($T:ty, $filename:literal) => {
        paste::paste! {
            #[derive(askama::Template)]
            #[template(path = $filename, escape = "none")]
            pub(crate) struct $T {
                #[allow(dead_code)]
                config: Rc<TemplateConfig>
            }

            #[allow(dead_code)]
            impl $T {
                pub(crate) fn new(config: Rc<TemplateConfig>) -> Self {
                    Self { config }
                }
                pub(crate) fn rc_new(config: Rc<TemplateConfig>) -> Rc<dyn RenderedFile> {
                    Rc::new(Self::new(config.clone()))
                }
                fn project_root(&self) -> Utf8PathBuf {
                    self.config.project.project_root().into()
                }
            }
        }
    };
}

pub(crate) mod files {
    use std::rc::Rc;

    use super::{RenderedFile, TemplateConfig};
    #[cfg(feature = "wasm")]
    use crate::wasm;
    use crate::{config::Framework, jsi, Platform};

    pub(crate) fn get_files_for(
        config: Rc<TemplateConfig>,
        platform: &Platform,
    ) -> Vec<Rc<dyn RenderedFile>> {
        let mut files = vec![];
        // Nitro and TurboModule emit entirely different platform shells. We
        // dispatch up-front so the two paths never accidentally co-emit.
        let is_nitro = matches!(config.project.framework, Framework::Nitro);
        match platform {
            Platform::Android => {
                if is_nitro {
                    files.extend(jsi::nitro::get_files_for_android(config.clone()));
                } else {
                    files.extend(jsi::crossplatform::get_files(config.clone()));
                    files.extend(jsi::android::get_files(config.clone()));
                }
            }
            Platform::Ios => {
                if is_nitro {
                    files.extend(jsi::nitro::get_files_for_ios(config.clone()));
                } else {
                    files.extend(jsi::crossplatform::get_files(config.clone()));
                    files.extend(jsi::ios::get_files(config.clone()));
                }
            }
            #[cfg(feature = "wasm")]
            Platform::Wasm => {
                files.extend(wasm::get_files(config.clone()));
            }
        }
        files
    }

    pub(crate) fn get_files(config: Rc<TemplateConfig>) -> Vec<Rc<dyn RenderedFile>> {
        let mut files = vec![];
        if matches!(config.project.framework, Framework::Nitro) {
            files.extend(jsi::nitro::get_files(config.clone()));
        } else {
            files.extend(jsi::get_files(config.clone()));
        }
        #[cfg(feature = "wasm")]
        files.extend(wasm::get_files(config.clone()));
        files
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::rust_crate::CrateConfig,
        config::{self, BindingsConfig, ExtraArgs, Framework},
        jsi::android::config::AndroidConfig,
        jsi::crossplatform::TurboModulesConfig,
        jsi::ios::config::IOsConfig,
    };

    use super::*;

    impl ProjectConfig {
        pub(crate) fn empty(name: &str, crate_: CrateConfig) -> Self {
            let android = AndroidConfig {
                directory: "android".to_string(),
                jni_libs: "src/main/jniLibs".to_string(),
                targets: Default::default(),
                cargo_extras: ExtraArgs::default(),
                api_level: 21,
                package_name: "com.tester".to_string(),
                codegen_output_dir: "android/generated".to_string(),
                use_shared_library: false,
            };
            let ios = IOsConfig {
                directory: "ios".to_string(),
                framework_name: "MyRustCrateFramework".to_string(),
                xcodebuild_extras: ExtraArgs::default(),
                targets: Default::default(),
                cargo_extras: ExtraArgs::default(),
                codegen_output_dir: "ios/generated".to_string(),
            };
            let bindings = BindingsConfig {
                cpp: "cpp/bindings".to_string(),
                ts: "src/bindings".to_string(),
                uniffi_toml: Default::default(),
            };
            let tm = TurboModulesConfig {
                name: "MyCrateSpec".to_string(),
                cpp: "cpp".to_string(),
                ts: "src".to_string(),
                spec_name: "MyRustCrate".to_string(),
                entrypoint: "index.react-native.tsx".to_string(),
            };
            let repository = format!("https://github.com/user/{name}");

            #[cfg(feature = "wasm")]
            let wasm = crate::wasm::WasmConfig::default();

            Self {
                name: name.to_string(),
                project_version: "0.1.0".to_string(),
                framework: Framework::TurboModule,
                repository,
                crate_,
                android,
                ios,
                #[cfg(feature = "wasm")]
                wasm,
                bindings,
                tm,
                exclude_files: Default::default(),
            }
        }
    }

    fn create_template_config(name: &str, modules: &[&str]) -> Result<Rc<TemplateConfig>> {
        let manifest_dir: Utf8PathBuf = std::env::var("CARGO_MANIFEST_DIR").unwrap().into();
        let crate_metadata = CrateMetadata::try_from(manifest_dir.clone())?;
        let crate_config: CrateConfig = crate_metadata.clone().try_into()?;
        assert_eq!("crates/ubrn_cli/Cargo.toml", crate_config.manifest_path);
        assert_eq!(
            manifest_dir.join("Cargo.toml"),
            crate_config.manifest_path()?
        );

        let project_config = config::ProjectConfig::empty(name, crate_config);
        let modules = modules.iter().map(|s| ModuleMetadata::new(s)).collect();
        let template =
            TemplateConfig::new(project_config, crate_metadata, modules, Vec::new(), false);
        Ok(Rc::new(template))
    }

    // Regression test for https://github.com/jhugman/uniffi-bindgen-react-native/issues/315
    // When users configure paths with a leading "./" (e.g. `cpp: "./cpp"`), relative_to
    // must still produce canonical paths like "../cpp", not ".././cpp".
    #[test]
    fn test_relative_to_canonicalizes_dot_prefix_paths() -> Result<()> {
        // Define a test RenderedFile whose path is in android/ (like CMakeLists.txt)
        templated_file!(AndroidFile, "TemplateTester.txt");
        impl RenderedFile for AndroidFile {
            fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
                self.config
                    .project
                    .android
                    .directory(project_root)
                    .join("CMakeLists.txt")
            }
        }

        let manifest_dir: Utf8PathBuf = std::env::var("CARGO_MANIFEST_DIR").unwrap().into();
        let crate_metadata = CrateMetadata::try_from(manifest_dir.clone())?;
        let crate_config: CrateConfig = crate_metadata.clone().try_into()?;

        let mut project_config = config::ProjectConfig::empty("test-path-fix", crate_config);
        // Simulate a user who wrote `cpp: "./cpp"` or `cpp: "./cpp/bindings"` in their config
        project_config.bindings.cpp = "./cpp/bindings".to_string();
        project_config.tm.cpp = "./cpp".to_string();

        let config = Rc::new(TemplateConfig::new(
            project_config,
            crate_metadata,
            vec![],
            Vec::new(),
            false,
        ));
        let file = AndroidFile::new(config.clone());
        let real_root = file.project_root();

        let bindings_abs = file.config.project.bindings.cpp_path(&real_root);
        let bindings_rel = file.relative_to(&real_root, &bindings_abs);
        // Canonical: "../cpp/bindings", not ".././cpp/bindings"
        assert_eq!(bindings_rel.as_str(), "../cpp/bindings");

        let tm_abs = file.config.project.tm.cpp_path(&real_root);
        let tm_rel = file.relative_to(&real_root, &tm_abs);
        // Canonical: "../cpp", not ".././cpp"
        assert_eq!(tm_rel.as_str(), "../cpp");

        Ok(())
    }

    #[test]
    fn test_templating() -> Result<()> {
        templated_file!(TemplateTester, "TemplateTester.txt");
        impl RenderedFile for TemplateTester {
            fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
                let name = self.config.project.module_cpp();
                let filename = format!("{name}.txt");
                self.config
                    .project
                    .android
                    .codegen_package_dir(project_root)
                    .join(filename)
            }
        }

        let config =
            create_template_config("my-tester-template-project", &["alice", "bob", "charlie"])?;
        let file = TemplateTester::new(config.clone());
        let project_root = Utf8PathBuf::new();
        let s = file.dyn_render()?;
        assert_eq!(
            "android/src/main/java/com/tester/MyTesterTemplateProject.txt".to_string(),
            file.path(&project_root).to_string()
        );
        // This is hard coded into the file. If this isn't here, then the test file hasn't rendered.
        assert!(s.contains("hardcoded into template."));
        assert_eq!(
            config.project.module_cpp(),
            "MyTesterTemplateProject".to_string()
        );
        assert!(s.contains("module_cpp = MyTesterTemplateProject."));
        assert!(s.contains("list of modules = ['NativeAlice', 'NativeBob', 'NativeCharlie']"));
        Ok(())
    }

    // -----------------------------------------------------------------
    // Nitro backend tests
    //
    // The Nitro path emits two distinct file sets: (a) the platform-glue
    // scaffolding handled here in `jsi::nitro::codegen` — gradle, cmake,
    // podspec, manifest, cpp-adapter, kotlin Package, JS entrypoint,
    // nitro.json, react-native.config.js — and (b) the per-uniffi-
    // interface HybridObject specs + impls handled by `ubrn_bindgen`'s
    // `gen_nitro` module. Only the platform glue is tested here; the
    // per-interface emission tests live next to that codegen.
    // -----------------------------------------------------------------

    use crate::jsi::nitro;
    use serde::de::value::{Error as SerdeValueError, StrDeserializer};
    use serde::Deserialize;

    fn parse_framework(input: &str) -> std::result::Result<Framework, SerdeValueError> {
        Framework::deserialize(StrDeserializer::<SerdeValueError>::new(input))
    }

    fn nitro_template_config(name: &str, modules: &[&str]) -> Result<Rc<TemplateConfig>> {
        nitro_template_config_with_hybrids(name, modules, &[])
    }

    /// Build a Nitro [`TemplateConfig`] with an explicit set of
    /// HybridObject entries. Tests use the `(ts_name, cxx_class)` pair
    /// to exercise the dynamic `nitro.json` autolinking + CMake source
    /// list emission. Always includes the namespace API entry so the
    /// shape matches what `gen_nitro::generate_all` actually produces.
    fn nitro_template_config_with_hybrids(
        name: &str,
        modules: &[&str],
        hybrids: &[(&str, &str, ubrn_bindgen::HybridObjectKind)],
    ) -> Result<Rc<TemplateConfig>> {
        let cfg = create_template_config(name, modules)?;
        let mut project = cfg.project.clone();
        project.framework = Framework::Nitro;
        let nitro_hybrid_objects = hybrids
            .iter()
            .map(|(name, cxx_class, kind)| ubrn_bindgen::HybridObjectEntry {
                name: (*name).to_string(),
                cxx_class: (*cxx_class).to_string(),
                // Test-only stub — the cxx_namespace field landed when the
                // `register_natives.cpp` emitter started fully qualifying
                // the impl class. Hardcoded here to a sentinel so the
                // template tests don't have to reason about namespaces.
                cxx_namespace: "test".to_string(),
                kind: *kind,
            })
            .collect();
        Ok(Rc::new(TemplateConfig::new(
            project,
            cfg.rust_crate.clone(),
            cfg.modules.clone(),
            nitro_hybrid_objects,
            cfg.native_bindings,
        )))
    }

    /// Mirrors render_files's filter step: a file declaring filter_by() == false
    /// is silently skipped at emit time. Tests that compare path sets must too.
    fn emitted_paths(files: &[Rc<dyn RenderedFile>]) -> Vec<String> {
        let root = Utf8PathBuf::from("");
        files
            .iter()
            .filter(|f| f.filter_by())
            .map(|f| f.path(&root).to_string())
            .collect::<Vec<_>>()
    }

    #[test]
    fn test_framework_serde_accepts_kebab_and_camel_aliases() {
        assert_eq!(parse_framework("nitro").unwrap(), Framework::Nitro);
        assert_eq!(
            parse_framework("turbo-module").unwrap(),
            Framework::TurboModule
        );
        assert_eq!(parse_framework("nitroModules").unwrap(), Framework::Nitro);
        assert_eq!(parse_framework("nitro-native").unwrap(), Framework::Nitro);
        assert_eq!(
            parse_framework("turboModule").unwrap(),
            Framework::TurboModule
        );
        assert_eq!(
            parse_framework("turbomodule").unwrap(),
            Framework::TurboModule
        );
        assert!(parse_framework("napi").is_err());
        assert!(parse_framework("").is_err());
    }

    #[test]
    fn test_framework_defaults_to_turbo_module() {
        assert_eq!(Framework::default(), Framework::TurboModule);
    }

    #[test]
    fn test_nitro_cross_platform_emits_exact_path_set() -> Result<()> {
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let mut paths = emitted_paths(&nitro::codegen::get_files(config));
        paths.sort();
        // Cross-platform glue: JS entrypoint, autolinking manifest, RN
        // CLI hint. Per-interface HybridObject TS/C++ is NOT emitted by
        // this layer — that's gen_nitro in ubrn_bindgen.
        assert_eq!(
            paths,
            vec![
                "index.react-native.tsx".to_string(),
                "nitro.json".to_string(),
                "react-native.config.js".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_nitro_android_emit_adds_full_platform_glue() -> Result<()> {
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let cross: Vec<String> = emitted_paths(&nitro::codegen::get_files(config.clone()));
        let all: Vec<String> = emitted_paths(&nitro::codegen::get_files_for_android(config));
        let mut added: Vec<String> = all.into_iter().filter(|p| !cross.contains(p)).collect();
        added.sort();
        // build.gradle / CMakeLists / cpp-adapter / kotlin Package are
        // load-bearing on Android; manifest is a one-liner namespace
        // declaration; proguard-rules is gated on native_bindings so it's
        // absent here.
        assert_eq!(
            added,
            vec![
                "android/CMakeLists.txt".to_string(),
                "android/build.gradle".to_string(),
                "android/cpp-adapter.cpp".to_string(),
                "android/src/main/AndroidManifest.xml".to_string(),
                "android/src/main/java/com/margelo/nitro/acme/AcmePackage.kt".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_nitro_ios_emit_adds_only_podspec() -> Result<()> {
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let cross: Vec<String> = emitted_paths(&nitro::codegen::get_files(config.clone()));
        let all: Vec<String> = emitted_paths(&nitro::codegen::get_files_for_ios(config));
        let added: Vec<String> = all.into_iter().filter(|p| !cross.contains(p)).collect();
        assert_eq!(added, vec!["Acme.podspec".to_string()]);
        Ok(())
    }

    #[test]
    fn test_nitro_index_tsx_re_exports_native_bindings() -> Result<()> {
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let file = nitro::codegen::IndexTsx::new(config.clone());
        let s = file.dyn_render()?;
        // The entrypoint re-exports the gen_nitro-emitted TS modules. It
        // does NOT instantiate any installer — there is no installer in
        // the native path. JS just imports the HybridObjects directly.
        assert!(s.contains("export"));
        assert!(s.contains("alice"));
        // Re-export must be NAMESPACED (`export * as <ns>`), not flat
        // (`export *`): each RPC-service namespace independently defines
        // same-named request / result DTOs, so a flat re-export collides
        // (TS2308 "already exported a member"). Pin the namespaced form so a
        // regression back to flat `export *` is caught.
        assert!(
            s.contains("export * as alice from"),
            "nitro index.tsx must namespace each module re-export, got:\n{s}"
        );
        // No TurboModuleRegistry leakage, no installer-style methods.
        assert!(!s.contains("TurboModuleRegistry"));
        assert!(!s.contains("installer.install"));
        assert!(!s.contains("installRustCrate"));
        Ok(())
    }

    #[test]
    fn test_nitro_cpp_adapter_calls_register_all_natives() -> Result<()> {
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let file = nitro::codegen::NitroCppAdapter::new(config.clone());
        let s = file.dyn_render()?;
        // JNI_OnLoad fires when System.loadLibrary completes. It initializes
        // fbjni and returns JNI_VERSION_1_6 so loadLibrary succeeds.
        assert!(s.contains("JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM* vm, void*)"));
        assert!(s.contains("facebook::jni::initialize(vm,"));
        // No nitrogen OnLoad / registerAllNatives — registration happens at
        // library-load time via the static initializer in register_natives.cpp.
        assert!(!s.contains("registerAllNatives"));
        assert!(!s.contains("OnLoad.hpp"));
        assert!(!s.contains("nitrogen/generated"));
        assert_eq!(
            file.path(&Utf8PathBuf::new()).as_str(),
            "android/cpp-adapter.cpp"
        );
        Ok(())
    }

    #[test]
    fn test_nitro_package_kt_triggers_load_library_via_companion_init() -> Result<()> {
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let file = nitro::codegen::NitroPackageKt::new(config.clone());
        let s = file.dyn_render()?;
        assert!(s.contains("package com.margelo.nitro.acme"));
        assert!(s.contains("class AcmePackage : BaseReactPackage()"));
        assert!(s.contains("companion object {"));
        assert!(s.contains("init {"));
        // System.loadLibrary maps the .so in, which fires the static
        // initializer in register_natives.cpp. No nitrogen OnLoad indirection.
        assert!(s.contains("System.loadLibrary(\"Acme\")"));
        assert!(!s.contains("AcmeOnLoad.initializeNative()"));
        assert!(!s.contains("nitrogen/generated"));
        assert_eq!(
            file.path(&Utf8PathBuf::new()).as_str(),
            "android/src/main/java/com/margelo/nitro/acme/AcmePackage.kt"
        );
        Ok(())
    }

    #[test]
    fn test_nitro_react_native_config_points_at_kotlin_package() -> Result<()> {
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let file = nitro::codegen::ReactNativeConfig::new(config.clone());
        let s = file.dyn_render()?;
        assert!(s.contains("module.exports = {"));
        assert!(s.contains("packageImportPath: 'import com.margelo.nitro.acme.AcmePackage;'"));
        assert!(s.contains("packageInstance: 'new AcmePackage()'"));
        assert!(s.contains("sourceDir: './android'"));
        assert!(s.contains(".podspec"));
        Ok(())
    }

    #[test]
    fn test_nitro_build_gradle_applies_autolinking() -> Result<()> {
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let file = nitro::codegen::NitroBuildGradle::new(config.clone());
        let s = file.dyn_render()?;
        // No nitrogen autolinking apply — the C++ runtime is linked from
        // CMakeLists.txt and registration happens at library-load time.
        assert!(!s.contains("nitrogen/generated"));
        assert!(!s.contains("+autolinking.gradle"));
        // The Nitro Modules runtime dependency stays.
        assert!(s.contains("implementation project(\":react-native-nitro-modules\")"));
        // TurboModule codegen plumbing must not leak in.
        assert!(!s.contains("isNewArchitectureEnabled"));
        assert!(!s.contains("apply plugin: \"com.facebook.react\""));
        assert!(!s.contains("libraryName"));
        // proguard-rules.pro is gated on native_bindings (JNA keep-rules) — it
        // is NOT emitted for this config (native_bindings=false), so the
        // `consumerProguardFiles` reference must also be absent. Referencing a
        // file that wasn't shipped makes Gradle fail with "Supplied consumer
        // proguard configuration does not exist".
        assert!(!s.contains("consumerProguardFiles"));
        Ok(())
    }

    #[test]
    fn test_nitro_podspec_loads_add_nitrogen_files() -> Result<()> {
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let file = nitro::codegen::NitroPodspec::new(config.clone());
        let s = file.dyn_render()?;
        // No nitrogen autolinking load / add_nitrogen_files — we compile our
        // own sources and register at library-load time.
        assert!(!s.contains("nitrogen/generated"));
        assert!(!s.contains("+autolinking.rb"));
        assert!(!s.contains("add_nitrogen_files"));
        // The Nitro Modules runtime pod dependency stays.
        assert!(s.contains("s.dependency \"NitroModules\""));
        assert!(s.contains("vendored_frameworks ="));
        // TurboModule-era fallbacks must not bring in turbomodule/core.
        assert!(!s.contains("ReactCommon/turbomodule/core"));
        assert!(!s.contains("React-Codegen"));
        // The generated C++ angle-includes ubrn runtime headers without a
        // pod-name prefix (<RustBuffer.h>, <NitroUniffi.hpp>, <nitro-uniffi/...>),
        // which live in the uniffi-bindgen-react-native pod. The podspec must put
        // that pod's public-headers dir on HEADER_SEARCH_PATHS so they resolve on
        // iOS, preserving inherited paths.
        assert!(s.contains("HEADER_SEARCH_PATHS"));
        assert!(s.contains("Headers/Public/uniffi-bindgen-react-native"));
        assert!(s.contains("$(inherited)"));
        // The -fno-standalone-debug workaround is REMOVED: the iOS amalgamation
        // (compiling K unity chunks instead of ~257 per-object TUs) is the real
        // fix for the libtool 4GB archive overflow, so full standalone debug info
        // is retained.
        assert!(!s.contains("-fno-standalone-debug"));
        assert!(!s.contains("OTHER_CPLUSPLUSFLAGS"));
        assert!(!s.contains("OTHER_CFLAGS"));
        // c++20 + interop settings the Nitro runtime needs stay.
        assert!(s.contains("\"CLANG_CXX_LANGUAGE_STANDARD\" => \"c++20\""));
        // iOS compiles ONLY the amalgam chunks + register_natives.cpp, not the
        // per-object Hybrid*.cpp: source_files names the amalgam dir + headers.
        assert!(s.contains("amalgam/*.cpp"));
        assert!(s.contains("register_natives.cpp"));
        assert!(s.contains("{hpp,h}"));
        assert!(!s.contains("/**/*.{hpp,cpp,c,h}"));
        Ok(())
    }

    // -----------------------------------------------------------------
    // Dynamic Nitro emission tests
    //
    // These verify the integration loop: `gen_nitro::generate_all`
    // returns a `NitroEmission` with `hybrid_objects`; ubrn_cli plumbs
    // that list into `TemplateConfig::nitro_hybrid_objects`; the
    // `nitro.json` and `nitro-CMakeLists.txt` templates iterate it to
    // emit autolinking entries + the C++ source list. The legacy
    // `Installer` HybridObject (single hard-coded entry) must NOT
    // appear — it's superseded by the per-namespace + per-interface
    // entries the gen_nitro backend emits.
    // -----------------------------------------------------------------

    use ubrn_bindgen::HybridObjectKind;

    #[test]
    fn test_nitro_json_autolinks_every_emitted_hybrid_object() -> Result<()> {
        let config = nitro_template_config_with_hybrids(
            "react-native-acme",
            &["alice"],
            &[
                ("AliceApi", "HybridAliceApi", HybridObjectKind::NamespaceApi),
                ("Greeter", "HybridGreeter", HybridObjectKind::Interface),
                ("Counter", "HybridCounter", HybridObjectKind::Interface),
            ],
        )?;
        let file = nitro::codegen::NitroJson::new(config.clone());
        let s = file.dyn_render()?;

        // Each HybridObject autolinks under language: c++ with the
        // exact (ts_name -> cxx_class) pair gen_nitro emitted.
        assert!(s.contains("\"AliceApi\""), "AliceApi entry: {s}");
        assert!(
            s.contains("\"implementationClassName\": \"HybridAliceApi\""),
            "AliceApi impl: {s}"
        );
        assert!(s.contains("\"Greeter\""), "Greeter entry: {s}");
        assert!(
            s.contains("\"implementationClassName\": \"HybridGreeter\""),
            "Greeter impl: {s}"
        );
        assert!(s.contains("\"Counter\""), "Counter entry: {s}");
        assert!(
            s.contains("\"implementationClassName\": \"HybridCounter\""),
            "Counter impl: {s}"
        );

        // No leftover static Installer entry.
        assert!(
            !s.contains("\"AcmeInstaller\""),
            "stale Installer autolink leaked: {s}"
        );
        assert!(
            !s.contains("HybridAcmeInstaller"),
            "stale Installer impl class leaked: {s}"
        );

        // Sanity: the cxxNamespace + iosModuleName scaffolding is still
        // there (project-derived, not from hybrid_objects).
        assert!(s.contains("\"cxxNamespace\": [\"acme\"]"));
        assert!(s.contains("\"iosModuleName\": \"Acme\""));

        // Must be valid JSON.
        let parsed: serde_json::Value =
            serde_json::from_str(&s).expect("nitro.json must parse as JSON");
        let autolinking = parsed
            .get("autolinking")
            .and_then(|a| a.as_object())
            .expect("autolinking is an object");
        assert_eq!(
            autolinking.len(),
            3,
            "expected exactly 3 autolinking entries, got {autolinking:#?}"
        );
        Ok(())
    }

    #[test]
    fn test_nitro_json_empty_when_no_hybrid_objects() -> Result<()> {
        // `generate jsi nitro` and other "templates-only" entrypoints
        // emit nitro.json without first running gen_nitro. The result
        // must still be valid JSON with an empty autolinking block.
        let config = nitro_template_config("react-native-acme", &["alice"])?;
        let file = nitro::codegen::NitroJson::new(config.clone());
        let s = file.dyn_render()?;
        let parsed: serde_json::Value =
            serde_json::from_str(&s).expect("nitro.json must parse as JSON");
        let autolinking = parsed
            .get("autolinking")
            .and_then(|a| a.as_object())
            .expect("autolinking is an object");
        assert!(autolinking.is_empty());
        Ok(())
    }

    #[test]
    fn test_nitro_cmakelists_lists_every_hybrid_cpp() -> Result<()> {
        let config = nitro_template_config_with_hybrids(
            "react-native-acme",
            &["alice"],
            &[
                ("AliceApi", "HybridAliceApi", HybridObjectKind::NamespaceApi),
                ("Greeter", "HybridGreeter", HybridObjectKind::Interface),
            ],
        )?;
        let file = nitro::codegen::NitroCMakeLists::new(config.clone());
        let s = file.dyn_render()?;

        // The default project layout in ProjectConfig::empty puts the
        // bindings at `cpp/bindings` — the CMakeLists is at
        // `android/CMakeLists.txt` so the relative path back is
        // `../cpp/bindings`.
        assert!(
            s.contains("../cpp/bindings/HybridAliceApi.cpp"),
            "HybridAliceApi source missing: {s}"
        );
        assert!(
            s.contains("../cpp/bindings/HybridGreeter.cpp"),
            "HybridGreeter source missing: {s}"
        );

        // The legacy hard-coded Installer source line must be gone.
        assert!(
            !s.contains("HybridAcmeInstaller.cpp"),
            "stale Installer source leaked: {s}"
        );
        // ...and the legacy per-namespace ts-derived cpp file too.
        assert!(
            !s.contains("/alice.cpp"),
            "stale per-namespace `.cpp` source leaked: {s}"
        );

        // cpp-adapter.cpp must still be in the add_library — it carries
        // JNI_OnLoad.
        assert!(s.contains("cpp-adapter.cpp"));
        // register_natives.cpp (the static-init registration) is compiled too.
        assert!(s.contains("register_natives.cpp"));
        // No nitrogen autolinking include — we link the runtime directly via
        // find_package(react-native-nitro-modules) instead.
        assert!(!s.contains("nitrogen/generated"));
        assert!(!s.contains("+autolinking.cmake"));
        assert!(s.contains("react-native-nitro-modules::NitroModules"));
        Ok(())
    }

    #[test]
    fn test_nitro_json_emission_is_deterministic() -> Result<()> {
        // Re-running the bindings step must produce byte-identical
        // output — the platform-glue layer feeds Nitrogen, which feeds
        // CMake, and any churn would invalidate the build cache. The
        // gen_nitro backend already de-duplicates via BTreeSet but the
        // sort in `TemplateConfig::new` is the belt-and-braces
        // guarantee against upstream HashSet contamination.
        let hybrids = [
            ("ZebraApi", "HybridZebraApi", HybridObjectKind::NamespaceApi),
            ("Apple", "HybridApple", HybridObjectKind::Interface),
            ("Mango", "HybridMango", HybridObjectKind::Interface),
        ];
        let cfg1 = nitro_template_config_with_hybrids("react-native-acme", &["alice"], &hybrids)?;
        let cfg2 = nitro_template_config_with_hybrids("react-native-acme", &["alice"], &hybrids)?;
        let s1 = nitro::codegen::NitroJson::new(cfg1).dyn_render()?;
        let s2 = nitro::codegen::NitroJson::new(cfg2).dyn_render()?;
        assert_eq!(s1, s2);

        // Reordered input produces identical output.
        let mut shuffled = hybrids.to_vec();
        shuffled.reverse();
        let cfg3 = nitro_template_config_with_hybrids("react-native-acme", &["alice"], &shuffled)?;
        let s3 = nitro::codegen::NitroJson::new(cfg3).dyn_render()?;
        assert_eq!(s1, s3, "emission must be invariant under input ordering");
        Ok(())
    }
}
