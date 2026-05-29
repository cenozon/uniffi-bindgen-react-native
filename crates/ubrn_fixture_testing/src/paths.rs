/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
use camino::{Utf8Path, Utf8PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use ubrn_common::nitro;

use crate::metadata;

/// Repository root, derived from workspace metadata.
pub(crate) fn repo_root() -> &'static Utf8Path {
    static ROOT: LazyLock<Utf8PathBuf> = LazyLock::new(|| {
        let meta = metadata::workspace_metadata();
        meta.workspace_root.clone()
    });
    &ROOT
}

pub(crate) fn build_root() -> Utf8PathBuf {
    repo_root().join("build")
}

pub(crate) fn node_modules_bin() -> Utf8PathBuf {
    repo_root().join("node_modules").join(".bin")
}

pub(crate) fn hermes_src_dir() -> Utf8PathBuf {
    repo_root().join("cpp_modules").join("hermes")
}

pub(crate) fn hermes_build_dir() -> Utf8PathBuf {
    build_root().join("hermes")
}

pub(crate) fn test_runner_binary() -> Utf8PathBuf {
    let dir = build_root().join("test-runner");
    if cfg!(target_os = "windows") {
        dir.join("Debug").join("test-runner.exe")
    } else {
        dir.join("test-runner")
    }
}

/// Panics with a helpful message if required bootstrap artifacts are missing.
pub(crate) fn assert_jsi_bootstrap() {
    let runner = test_runner_binary();
    assert!(
        runner.exists(),
        "Hermes test-runner not found at {runner}. Run `cargo xtask bootstrap` first."
    );
    let hermes = hermes_build_dir();
    assert!(
        hermes.exists(),
        "Hermes build not found at {hermes}. Run `cargo xtask bootstrap` first."
    );
    assert_node_modules();
}

pub(crate) fn assert_wasm_bootstrap() {
    assert_node_modules();
}

// === Nitro paths ===

/// Nitro's C++ source root (`cpp/`) inside the resolved
/// `react-native-nitro-modules` package — the include/compile root for the
/// per-fixture glue. Resolved from `node_modules` (or the `UBRN_NITRO_LOCAL`
/// override) via [`ubrn_common::nitro`], exactly like the `xtask` build steps.
///
/// Only called after [`assert_nitro_bootstrap`] has confirmed the package
/// resolves, so the `expect` is unreachable in the test flow.
pub(crate) fn nitro_cpp_src_dir() -> Utf8PathBuf {
    nitro::cpp_dir(repo_root())
        .expect("react-native-nitro-modules cpp/ (checked by assert_nitro_bootstrap)")
}

/// The `UBRN_NITRO_LOCAL` override package directory, if set.
///
/// When `None` (the common case), `react-native-nitro-modules` is installed
/// in `node_modules` and Metro/tsc resolve it natively — no remapping needed.
/// When `Some`, the package lives outside the module tree, so callers must
/// register it as a Metro `extraNodeModules` + tsc `paths` entry. The path is
/// already canonicalized by [`nitro::local_override`], so it
/// never reintroduces a symlink for Metro to choke on.
pub(crate) fn nitro_local_override() -> Option<Utf8PathBuf> {
    nitro::local_override().ok().flatten()
}

/// Build directory hosting `libNitroModules.{so,dylib,dll}` — written
/// by `xtask bootstrap nitro`.
pub(crate) fn nitro_build_dir() -> Utf8PathBuf {
    build_root().join("nitro-build")
}

/// Public include root the nitro bootstrap populates with a flat
/// `NitroModules/` directory of every upstream `.hpp` (symlinks on Unix,
/// copies on Windows). Consumer cmake projects add this so generated
/// sources can resolve `<NitroModules/Foo.hpp>`-style includes.
pub(crate) fn nitro_flat_include_dir() -> Utf8PathBuf {
    nitro_build_dir().join("include")
}

/// Platform-specific path to the shared NitroModules library that the
/// per-fixture cdylib (and the test-runner) dynamically loads.
pub(crate) fn nitro_lib_path() -> Utf8PathBuf {
    let dir = nitro_build_dir();
    if cfg!(target_os = "windows") {
        dir.join("Debug").join("NitroModules.dll")
    } else if cfg!(target_os = "macos") {
        dir.join("libNitroModules.dylib")
    } else {
        dir.join("libNitroModules.so")
    }
}

/// The Hermes-aware host Nitro test-runner. Built by
/// `xtask bootstrap nitro-test-runner` into `<build_root>/test-runner-nitro/`.
pub(crate) fn nitro_test_runner_binary() -> Utf8PathBuf {
    let dir = build_root().join("test-runner-nitro");
    if cfg!(target_os = "windows") {
        dir.join("Debug").join("test-runner-nitro.exe")
    } else {
        dir.join("test-runner-nitro")
    }
}

/// Verify every bootstrap artifact needed to run a Nitro fixture test.
///
/// On missing pieces, returns a descriptive `Err` so callers can *skip*
/// (rather than fail) the test. We deliberately don't `panic!` here —
/// the Nitro runner is gated behind extra `xtask bootstrap` work that a
/// fresh `cargo test --workspace` won't have run.
pub(crate) fn assert_nitro_bootstrap() -> Result<(), String> {
    let runner = nitro_test_runner_binary();
    if !runner.exists() {
        return Err(format!(
            "Nitro test-runner not found at {runner}. \
             Run `cargo xtask bootstrap nitro-test-runner` first."
        ));
    }
    let lib = nitro_lib_path();
    if !lib.exists() {
        return Err(format!(
            "NitroModules library not found at {lib}. \
             Run `cargo xtask bootstrap nitro` first."
        ));
    }
    // The Nitro package itself: installed under node_modules (run
    // `yarn install`) or pointed at by `UBRN_NITRO_LOCAL`, and it must carry
    // its `cpp/` source tree so the per-fixture glue can compile.
    if let Err(e) = nitro::cpp_dir(repo_root()) {
        return Err(e.to_string());
    }
    let nm = repo_root().join("node_modules");
    if !nm.exists() {
        return Err(format!(
            "node_modules not found at {nm}. Run `cargo xtask bootstrap` first."
        ));
    }
    Ok(())
}

/// On Windows, the dynamic loader needs the directory containing
/// `NitroModules.dll` (and Hermes' DLLs) on `PATH` at runtime, just like
/// the JSI flow's `add_hermes_dll_paths`. On Linux/macOS the `rpath` set
/// by the per-fixture CMakeLists handles this.
pub(crate) fn add_nitro_dll_paths(cmd: &mut Command) {
    if cfg!(target_os = "windows") {
        let nitro_dir = nitro_build_dir().join("Debug");
        let hermes_dll_dir = hermes_build_dir().join("API/hermes/Debug");
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{};{};{}", nitro_dir, hermes_dll_dir, path));
    }
}

/// On Windows, DLLs must be on PATH at runtime. This is not an issue on Linux/macOS as the
/// binary's rpath tells the linker where to find the shared libraries.
/// This adds the Hermes DLL directory to PATH on the given command.
pub(crate) fn add_hermes_dll_paths(cmd: &mut Command) {
    if cfg!(target_os = "windows") {
        let hermes_dll_dir = hermes_build_dir().join("API/hermes/Debug");
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{};{}", hermes_dll_dir, path));
    }
}

/// Path to the napi runtime directory within the repo.
pub(crate) fn napi_runtime_dir() -> Utf8PathBuf {
    repo_root().join("runtimes").join("napi")
}

pub(crate) fn assert_napi_bootstrap() {
    assert_node_modules();
    let napi_dir = napi_runtime_dir();
    let node_file = napi_dir.join(format!(
        "uniffi-runtime-napi.{}.node",
        napi_platform_triple()
    ));
    assert!(
        node_file.exists(),
        "N-API runtime not found at {node_file}. Run `cd runtimes/napi && npm run build:debug` first."
    );
}

fn napi_platform_triple() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-arm64-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x64-gnu"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "win32-x64-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "win32-arm64-msvc"
    } else {
        panic!("Unsupported platform for N-API tests")
    }
}

fn assert_node_modules() {
    let nm = repo_root().join("node_modules");
    assert!(
        nm.exists(),
        "node_modules not found at {nm}. Run `cargo xtask bootstrap` first."
    );
}
