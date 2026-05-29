/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */

//! Per-fixture Nitro test driver.
//!
//! Mirrors `jsi.rs` end-to-end:
//!   1. Verify bootstrap (test-runner-nitro, libNitroModules, nitro sources).
//!   2. Build the fixture's Rust cdylib.
//!   3. Emit Nitro TS + C++ bindings via `ubrn_bindgen::BindingsArgs::run`.
//!   4. CMake/Ninja a per-fixture .so containing every `Hybrid*.cpp` +
//!      `register_natives.cpp`, linked against the fixture cdylib + the
//!      bootstrapped NitroModules library.
//!   5. tsc + metro-bundle the test script (reusing the JSI pipeline,
//!      with `react-native-nitro-modules` registered as `extraNodeModules`).
//!   6. Invoke `test-runner-nitro <bundle.js> <fixture.so>`.
//!
//! A missing bootstrap artifact *skips* the test (`eprintln!` + early
//! return) rather than panicking, mirroring `crates/ubrn_bindgen/tests/
//! nitro_arithmetic.rs`. This keeps `cargo test --workspace` green on
//! machines that haven't run the extra `xtask bootstrap nitro` /
//! `bootstrap nitro-test-runner` steps.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};

use ubrn_bindgen::{AbiFlavor, BindingsArgs, OutputArgs, SourceArgs, SwitchArgs};

use crate::{metadata, paths, run_cmd, run_cmd_quietly, typescript};

/// Run a fixture test under the Nitro flavor.
///
/// Called from proc-macro-generated `#[test]` functions emitted by
/// `ubrn_macros::build_foreign_language_testcases!` when a fixture's
/// backend list contains `Nitro`.
pub fn run_test(crate_name: &str, test_script: &str, target_tmpdir: &str) {
    // Serialize with other flavors for this fixture (they share generated/).
    let _lock = crate::lock_fixture();

    // Step 0: Check bootstrap. Skip (not fail) on missing artifacts —
    // matches the pattern in `crates/ubrn_bindgen/tests/nitro_arithmetic.rs`.
    if let Err(msg) = paths::assert_nitro_bootstrap() {
        eprintln!("skipping nitro::{crate_name}: {msg}");
        return;
    }

    let test_script = Utf8Path::new(test_script);
    let test_stem = test_script.file_stem().unwrap_or("test");

    // Per-test output directory.
    let out_dir =
        Utf8PathBuf::from(target_tmpdir).join(format!("ubrn-tests/{crate_name}-{test_stem}-nitro"));
    std::fs::create_dir_all(&out_dir).expect("failed to create output dir");

    // Step 1: Build the fixture crate (so the cdylib + uniffi metadata
    // are both available for library-mode bindgen).
    crate::cargo_build(crate_name);

    // Step 2: Emit Nitro bindings.
    // Artifacts live under the fixture's `generated/nitro/` so test
    // scripts can resolve `@/generated/<namespace>` via the tsconfig
    // alias, parallel to the JSI flow.
    let lib_name = metadata::find_cdylib_name(crate_name);
    let cdylib_path = metadata::find_cdylib_from_name(&lib_name);
    let fixture_dir = metadata::find_package_dir(crate_name);
    let generated_nitro = fixture_dir.join("generated/nitro");
    let _ = std::fs::remove_dir_all(&generated_nitro);
    let ts_dir = generated_nitro.join("ts");
    let cpp_dir = generated_nitro.join("cpp");
    std::fs::create_dir_all(&ts_dir).expect("failed to create ts dir");
    std::fs::create_dir_all(&cpp_dir).expect("failed to create cpp dir");
    generate_bindings(&cdylib_path, &ts_dir, &cpp_dir);

    // Step 3: Compile the C++ glue into a per-fixture shared library.
    // The test-runner dlopens this and looks up `registerNatives`.
    let target_dir = &metadata::workspace_metadata().target_directory;
    let so_file = compile_cpp(&cpp_dir, &out_dir, &lib_name, target_dir);

    // Step 4: Bundle the TS test. `react-native-nitro-modules` is installed
    // in `node_modules`, so Metro/tsc resolve it natively — no remapping. Only
    // when a `UBRN_NITRO_LOCAL` override points outside the module tree do we
    // register it as a Metro `extraNodeModules` + TSC `paths` entry.
    //
    // `react-native-nitro-modules` statically requires `react-native` and
    // `react-native-worklets` (the latter inside a try/catch at runtime, but
    // Metro still chokes if it can't resolve the spec). Materialize tiny no-op
    // stub packages so the bundle resolves cleanly. Bind owned PathBufs first
    // so the `&Utf8Path` borrows below outlive the slice.
    let stubs_root = out_dir.join("stub_modules");
    let rn_stub = make_stub_module(&stubs_root, "react-native");
    let rn_worklets_stub = make_stub_module(&stubs_root, "react-native-worklets");
    let nitro_override = paths::nitro_local_override();
    let mut extras: Vec<(&str, &Utf8Path)> = vec![
        ("react-native", rn_stub.as_path()),
        ("react-native-worklets", rn_worklets_stub.as_path()),
    ];
    if let Some(ref pkg) = nitro_override {
        extras.push(("react-native-nitro-modules", pkg.as_path()));
    }
    let bundle = typescript::prepare_for_jsi_with_extras_and_platform(
        test_script,
        &out_dir,
        Some(&ts_dir),
        &extras,
        // `react-native-nitro-modules` ships a `.web.js` whose proxy
        // throws on every access. Pin Metro to a non-web platform so it
        // resolves to the bare `.js` that checks `global.NitroModulesProxy`
        // (which `installNitro()` set up in the host runner).
        Some("ios"),
    );

    // Step 5: Run test-runner-nitro.
    run_test_runner(&bundle, &so_file);
}

/// Drive `BindingsArgs::run` programmatically with `AbiFlavor::Nitro`.
///
/// We bypass the CLI because:
///   1. It avoids the cost of a `cargo run -p uniffi-bindgen-react-native`
///      rebuild per test (~tens of seconds on cold cache).
///   2. `BindingsArgs::run` is the same entry point that
///      `crates/ubrn_bindgen/tests/nitro_arithmetic.rs` exercises, so any
///      future change there flows through here uniformly.
fn generate_bindings(cdylib_path: &Utf8Path, ts_dir: &Utf8Path, cpp_dir: &Utf8Path) {
    let switches = SwitchArgs {
        flavor: AbiFlavor::Nitro,
    };
    let source = SourceArgs::library(&cdylib_path.to_path_buf());
    let output = OutputArgs::new(ts_dir, cpp_dir, /* no_format */ true);
    let args = BindingsArgs::new(switches, source, output);
    args.run(None).expect("nitro bindings emission");
}

/// Compile every `Hybrid*.cpp` + `register_natives.cpp` under `cpp_dir`
/// into a per-fixture shared library that the test-runner dlopens.
fn compile_cpp(
    cpp_dir: &Utf8Path,
    out_dir: &Utf8Path,
    lib_name: &str,
    target_dir: &Utf8Path,
) -> Utf8PathBuf {
    // Profile-qualified so a debug and a release run don't share one CMake
    // cache: `find_library(RUST_LIB_PATH …)` is cached on first configure and
    // not re-evaluated on reconfigure, so a shared dir would silently keep
    // linking whichever profile's cdylib was resolved first.
    let build_dir = out_dir.join(format!("cpp-build-{}", crate::fixture_profile_dir()));
    std::fs::create_dir_all(&build_dir).expect("failed to create cpp-build dir");

    // Collect every .cpp in the emitted dir. Includes:
    //  - HybridArithmeticApi.cpp (namespace API impl)
    //  - HybridXxx.cpp (one per uniffi interface)
    //  - register_natives.cpp (project-level autolinking)
    let mut cpp_files: Vec<String> = Vec::new();
    let read_dir =
        std::fs::read_dir(cpp_dir).unwrap_or_else(|e| panic!("failed to read {cpp_dir}: {e}"));
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("cpp") {
            let canonical = dunce::canonicalize(&path).expect("failed to canonicalize cpp path");
            cpp_files.push(canonical.to_string_lossy().to_string());
        }
    }
    cpp_files.sort();
    if cpp_files.is_empty() {
        panic!("no .cpp files found under {cpp_dir} — nitro emission produced nothing?");
    }

    // Write the CMakeLists into the build dir to keep generated files
    // out of the source tree (mirrors the nitro-build bootstrap strategy).
    let cmake_lists = write_cmake_lists(&build_dir, &cpp_files, cpp_dir, lib_name, target_dir);

    // cmake configure
    let mut cmd = Command::new("cmake");
    cmd.current_dir(&build_dir)
        .arg("-G")
        .arg(if cfg!(target_os = "windows") {
            "Visual Studio 16 2019"
        } else {
            "Ninja"
        })
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg(format!("-B{build_dir}"))
        .arg(cmake_lists.parent().unwrap().as_str());
    run_cmd_quietly(&mut cmd);

    // build
    if cfg!(target_os = "windows") {
        run_cmd_quietly(Command::new("cmake").arg("--build").arg(build_dir.as_str()));
    } else {
        run_cmd_quietly(Command::new("ninja").arg("-C").arg(build_dir.as_str()));
    }

    let ext = metadata::shared_lib_ext();
    let target_name = format!("nitro-{lib_name}");
    if cfg!(target_os = "windows") {
        build_dir.join(format!("Debug/{target_name}.{ext}"))
    } else {
        build_dir.join(format!("lib{target_name}.{ext}"))
    }
}

/// Write a CMakeLists.txt for the per-fixture shared library. We don't
/// reuse `cpp/hermes-rust-extension/CMakeLists.txt` because Nitro needs:
///   - `CMAKE_CXX_STANDARD 20` (vs 20 in jsi-rust-extension, but the
///     header set is wholly different — Nitro core + nitro-uniffi).
///   - The Nitro core headers + the in-tree nitro-uniffi headers
///     (`cpp/includes`) on the include path.
///   - The fixture cdylib *and* `libNitroModules` on the link line.
fn write_cmake_lists(
    build_dir: &Utf8Path,
    cpp_files: &[String],
    cpp_gen_dir: &Utf8Path,
    lib_name: &str,
    target_dir: &Utf8Path,
) -> Utf8PathBuf {
    let target_name = format!("nitro-{lib_name}");
    let cpp_files_list = cpp_files.join("\n    ");

    let hermes_src = paths::hermes_src_dir();
    let nitro_cpp_src = paths::nitro_cpp_src_dir();
    let nitro_lib_dir = paths::nitro_build_dir();
    let nitro_flat_include_dir = paths::nitro_flat_include_dir();
    let cpp_gen_dir_abs =
        dunce::canonicalize(cpp_gen_dir.as_std_path()).expect("failed to canonicalize cpp gen dir");
    let cpp_gen_dir_abs =
        Utf8PathBuf::from_path_buf(cpp_gen_dir_abs).expect("non-UTF-8 cpp gen path");
    let rust_target_dir = target_dir.join(crate::fixture_profile_dir());

    let cmake = format!(
        r#"# Auto-generated by ubrn_fixture_testing::nitro. Do not edit.
cmake_minimum_required(VERSION 3.22)
project({target_name} CXX)

# Nitro requires C++20.
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

if (CMAKE_CONFIGURATION_TYPES)
    set(HERMES_CONFIG_SUBDIR "/Debug")
else ()
    set(HERMES_CONFIG_SUBDIR "")
endif ()

set(SOURCES
    {cpp_files_list}
)

add_library({target_name} SHARED ${{SOURCES}})

target_include_directories({target_name} PRIVATE
    "{cpp_gen_dir_abs}"
    "{hermes_src}/API"
    "{hermes_src}/API/jsi"
    "{hermes_src}/public"
    # Nitro core headers, mirroring `xtask bootstrap nitro`'s include set.
    "{nitro_cpp_src}"
    "{nitro_cpp_src}/core"
    "{nitro_cpp_src}/entrypoint"
    "{nitro_cpp_src}/jsi"
    "{nitro_cpp_src}/platform"
    "{nitro_cpp_src}/prototype"
    "{nitro_cpp_src}/registry"
    "{nitro_cpp_src}/templates"
    "{nitro_cpp_src}/threading"
    "{nitro_cpp_src}/utils"
    "{nitro_cpp_src}/views"
    # Flat `NitroModules/` mirror dir — `xtask bootstrap nitro` populates
    # this with symlinks/copies of every Nitro .hpp so generated sources
    # can resolve `<NitroModules/Foo.hpp>`-style includes.
    "{nitro_flat_include_dir}"
    # In-tree shared headers + stubs: provides ReactCommon/CallInvoker.h and
    # the nitro-uniffi headers (`<NitroUniffi.hpp>` + `nitro-uniffi/*.hpp`)
    # that generated code includes.
    "{repo_root_cpp}/includes"
    "{repo_root_cpp}/stubs"
)

# Locate the fixture's freshly-built cdylib.
find_library(RUST_LIB_PATH NAMES {lib_name}
    PATHS "{rust_target_dir}" NO_DEFAULT_PATH)
if (NOT RUST_LIB_PATH)
    message(FATAL_ERROR "Could not find rust lib '{lib_name}' under '{rust_target_dir}'")
endif ()

# Locate the bootstrapped NitroModules shared library.
find_library(NITRO_LIB NAMES NitroModules
    PATHS "{nitro_lib_dir}" NO_DEFAULT_PATH)
if (NOT NITRO_LIB)
    message(FATAL_ERROR "Could not find NitroModules library under '{nitro_lib_dir}'")
endif ()

target_link_libraries({target_name} PRIVATE ${{RUST_LIB_PATH}} ${{NITRO_LIB}})

# Set rpath so the .so finds libNitroModules + the rust cdylib at load time.
if (APPLE)
    set_target_properties({target_name} PROPERTIES
        BUILD_RPATH "{nitro_lib_dir};{rust_target_dir}"
        INSTALL_RPATH "{nitro_lib_dir};{rust_target_dir}"
    )
elseif (UNIX)
    set_target_properties({target_name} PROPERTIES
        BUILD_RPATH "{nitro_lib_dir}:{rust_target_dir}"
        INSTALL_RPATH "{nitro_lib_dir}:{rust_target_dir}"
    )
endif ()

# On Windows, MSVC doesn't export DLL symbols by default — the test
# runner's `dlsym("registerNatives")` would fail without this.
set(CMAKE_WINDOWS_EXPORT_ALL_SYMBOLS ON)
if (MSVC)
    target_compile_definitions({target_name} PRIVATE NOMINMAX)
endif ()
"#,
        repo_root_cpp = paths::repo_root().join("cpp"),
    );

    let path = build_dir.join("CMakeLists.txt");
    std::fs::write(&path, cmake).expect("failed to write CMakeLists.txt");
    path
}

/// Materialize a no-op stub package at `<root>/<name>` so Metro can
/// resolve `require('<name>')` (and any sub-path require) to *something*
/// during bundling. The package exports an empty CommonJS object — both
/// `react-native` and `react-native-worklets` are only referenced inside
/// try/catch (or not exercised at all in the host tests), so an empty
/// surface is enough to keep the bundle resolution happy.
///
/// Extra subpaths can be requested to cover deep imports like
/// `react-native/Libraries/NativeComponent/NativeComponentRegistry`.
/// Each one materializes a passthrough file that re-exports the same
/// empty object.
fn make_stub_module(root: &Utf8Path, name: &str) -> Utf8PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("failed to create stub module dir");
    let pkg_json = format!(
        r#"{{
  "name": "{name}",
  "version": "0.0.0",
  "main": "index.js"
}}
"#
    );
    std::fs::write(dir.join("package.json"), pkg_json).expect("failed to write stub package.json");
    let stub_body = "module.exports = new Proxy({}, { get: () => () => {} });\n";
    std::fs::write(dir.join("index.js"), stub_body).expect("failed to write stub index.js");

    if name == "react-native" {
        // `react-native-nitro-modules` reaches into a couple of
        // deep-import paths at bundle-time even though the resulting
        // code never runs on the host. Materialize stubs for each.
        for sub in [
            "Libraries/NativeComponent/NativeComponentRegistry",
            "Libraries/NativeComponent/NativeComponentRegistry.js",
        ] {
            let sub_path = dir.join(sub);
            if let Some(parent) = sub_path.parent() {
                std::fs::create_dir_all(parent).expect("failed to create stub subdir");
            }
            std::fs::write(&sub_path, stub_body).expect("failed to write stub subpath");
        }
    }
    dir
}

/// Invoke the host Nitro test runner with the bundled JS + the per-
/// fixture shared library that exports `registerNatives`.
fn run_test_runner(bundle: &Utf8Path, so_file: &Utf8Path) {
    let runner = paths::nitro_test_runner_binary();
    let mut cmd = Command::new(runner.as_str());
    cmd.arg(bundle.as_str()).arg(so_file.as_str());
    paths::add_nitro_dll_paths(&mut cmd);
    run_cmd(&mut cmd);
}
