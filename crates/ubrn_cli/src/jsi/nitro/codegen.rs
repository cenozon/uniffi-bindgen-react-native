/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
//! Platform-glue file emission for the Nitro backend.
//!
//! What lives here: the per-project scaffolding that's invariant across
//! the uniffi metadata — `nitro.json`, `react-native.config.js`,
//! `android/build.gradle`, `android/CMakeLists.txt`,
//! `android/cpp-adapter.cpp`, the kotlin `<Name>Package.kt`, the iOS
//! podspec, the JS entrypoint, and the platform-neutral
//! `AndroidManifest.xml` / `proguard-rules.pro` we reuse from the legacy
//! templates dir.
//!
//! What does NOT live here: the per-interface / per-namespace HybridObject
//! C++ implementations and the `.nitro.ts` specs. Those depend on the
//! uniffi metadata and are emitted by `ubrn_bindgen`'s `gen_nitro` module
//! during the bindings phase, not the template phase.

use std::rc::Rc;

use camino::{Utf8Path, Utf8PathBuf};

use crate::codegen::{RenderedFile, TemplateConfig};
use crate::templated_file;

/// Cross-platform Nitro files: the JS entrypoint, the `nitro.json`
/// autolinking manifest, and the RN autolinker config. The per-interface
/// HybridObject specs are NOT emitted here — they're driven by uniffi
/// metadata in `crates/ubrn_bindgen/src/bindings/gen_nitro/`.
pub(crate) fn get_files(config: Rc<TemplateConfig>) -> Vec<Rc<dyn RenderedFile>> {
    vec![
        IndexTsx::rc_new(config.clone()),
        NitroJson::rc_new(config.clone()),
        ReactNativeConfig::rc_new(config.clone()),
    ]
}

/// Android emission for the Nitro path. Adds gradle / CMake / manifest /
/// proguard / cpp-adapter / kotlin Package on top of the cross-platform
/// set. `cpp-adapter.cpp` is what carries `JNI_OnLoad` →
/// `registerAllNatives()`; without it, every `createHybridObject` call
/// throws at runtime.
pub(crate) fn get_files_for_android(config: Rc<TemplateConfig>) -> Vec<Rc<dyn RenderedFile>> {
    let mut files = get_files(config.clone());
    files.push(NitroBuildGradle::rc_new(config.clone()));
    files.push(NitroCMakeLists::rc_new(config.clone()));
    files.push(AndroidManifest::rc_new(config.clone()));
    files.push(ProguardRules::rc_new(config.clone()));
    files.push(NitroCppAdapter::rc_new(config.clone()));
    files.push(NitroPackageKt::rc_new(config.clone()));
    files
}

/// iOS emission for the Nitro path. Adds the podspec on top of the
/// cross-platform set. The podspec calls Nitrogen's
/// `add_nitrogen_files(spec)` so all the `nitrogen/generated/` content
/// lands in the pod automatically.
pub(crate) fn get_files_for_ios(config: Rc<TemplateConfig>) -> Vec<Rc<dyn RenderedFile>> {
    let mut files = get_files(config.clone());
    files.push(NitroPodspec::rc_new(config.clone()));
    files
}

templated_file!(IndexTsx, "nitro-index.tsx");
impl RenderedFile for IndexTsx {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        self.config.project.tm.entrypoint(project_root)
    }
}

templated_file!(NitroJson, "nitro.json");
impl RenderedFile for NitroJson {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        project_root.join("nitro.json")
    }
}

templated_file!(NitroBuildGradle, "nitro-build.gradle");
impl RenderedFile for NitroBuildGradle {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        self.config
            .project
            .android
            .directory(project_root)
            .join("build.gradle")
    }
}

templated_file!(NitroCMakeLists, "nitro-CMakeLists.txt");
impl RenderedFile for NitroCMakeLists {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        self.config
            .project
            .android
            .directory(project_root)
            .join("CMakeLists.txt")
    }
}

// AndroidManifest and proguard-rules are platform-neutral — the same content
// works for TurboModule and Nitro builds. We reuse the existing askama
// templates rather than ship duplicates.
templated_file!(AndroidManifest, "AndroidManifest.xml");
impl RenderedFile for AndroidManifest {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        self.config
            .project
            .android
            .src_main_dir(project_root)
            .join("AndroidManifest.xml")
    }
}

templated_file!(ProguardRules, "proguard-rules.pro");
impl RenderedFile for ProguardRules {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        self.config
            .project
            .android
            .directory(project_root)
            .join("proguard-rules.pro")
    }
    fn filter_by(&self) -> bool {
        self.config.native_bindings
    }
}

templated_file!(NitroPodspec, "nitro-podspec.rb");
impl RenderedFile for NitroPodspec {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        let name = self.config.project.podspec_filename();
        let filename = format!("{name}.podspec");
        project_root.join(filename)
    }
}

// React Native CLI autolinker hint. Under Nitro, `packageImportPath` /
// `packageInstance` point at the kotlin `<Name>Package` whose
// `companion object { init { … } }` block dlopens the library; that is
// what triggers `JNI_OnLoad` → `registerAllNatives()` →
// `HybridObjectRegistry::registerHybridObjectConstructor`. RN still uses
// `sourceDir` / `podspecPath` for path discovery.
templated_file!(ReactNativeConfig, "nitro-react-native.config.js");
impl RenderedFile for ReactNativeConfig {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        project_root.join("react-native.config.js")
    }
}

// `cpp-adapter.cpp` defines `JNI_OnLoad`. On Android, this is the *only*
// way `registerAllNatives()` (and therefore the HybridObjectRegistry
// entries for every emitted interface + namespace HybridObject) gets
// invoked. Lives at `android/cpp-adapter.cpp` to match the path the nitro
// CMakeLists adds to `add_library(...)`.
templated_file!(NitroCppAdapter, "nitro-cpp-adapter.cpp");
impl RenderedFile for NitroCppAdapter {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        self.config
            .project
            .android
            .directory(project_root)
            .join("cpp-adapter.cpp")
    }
}

// `<Name>Package.kt` is a thin `BaseReactPackage` whose `companion object`
// runs `System.loadLibrary("<Name>")`. RN autolinking instantiates it via
// the `packageInstance` entry in `react-native.config.js`; the `init`
// block fires on first construction and triggers the dlopen → JNI_OnLoad
// → registerAllNatives chain.
templated_file!(NitroPackageKt, "nitro-Package.kt");
impl RenderedFile for NitroPackageKt {
    fn path(&self, project_root: &Utf8Path) -> Utf8PathBuf {
        let module_cpp = self.config.project.module_cpp();
        // Co-locate with the nitrogen-emitted `<Name>OnLoad.kt`, which
        // lives at `android/src/main/java/com/margelo/nitro/<ns>/…`.
        let ns = self.config.project.cpp_namespace();
        let filename = format!("{module_cpp}Package.kt");
        self.config
            .project
            .android
            .src_main_dir(project_root)
            .join("java")
            .join("com")
            .join("margelo")
            .join("nitro")
            .join(ns)
            .join(filename)
    }
}
