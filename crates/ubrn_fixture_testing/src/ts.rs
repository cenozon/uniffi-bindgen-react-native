/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
use std::process::Command;

use crate::{paths, run_cmd_quietly, typescript, Flavor};
use camino::{Utf8Path, Utf8PathBuf};

/// Run a framework TypeScript test (no fixture crate).
///
/// Called from proc-macro-generated `#[test]` functions.
pub fn run_test(test_script: &str, flavor: Flavor, target_tmpdir: &str) {
    let test_script = Utf8Path::new(test_script);
    let test_stem = test_script.file_stem().unwrap_or("test");
    let flavor_name = flavor.as_str();

    let out_dir =
        Utf8PathBuf::from(target_tmpdir).join(format!("ubrn-tests/ts-{test_stem}-{flavor_name}"));
    std::fs::create_dir_all(&out_dir).expect("failed to create output dir");

    match flavor {
        Flavor::Jsi => {
            paths::assert_jsi_bootstrap();
            // Serialize the entire JSI pipeline (compile + run): the Hermes
            // test-runner's timer implementation doesn't tolerate CPU
            // contention from parallel subprocesses (tsc, metro, other
            // test-runner instances), causing timing-sensitive tests to fail.
            let _lock = crate::lock_fixture();
            let bundle = typescript::prepare_for_jsi(test_script, &out_dir, None);
            run_test_runner_no_lib(&bundle);
        }
        Flavor::Wasm => {
            paths::assert_wasm_bootstrap();
            // WASM framework tests run directly via tsx
            crate::run_tsx(test_script);
        }
        Flavor::Napi => {
            paths::assert_napi_bootstrap();
            crate::run_tsx(test_script);
        }
        Flavor::Nitro => {
            // Framework TS tests don't have a fixture cdylib, so the
            // Nitro flavor reuses the JSI bundling pipeline + the
            // Nitro-aware test runner. Bootstrap checks here skip
            // (rather than panic) to mirror the per-fixture Nitro flow
            // — see `crate::nitro::run_test`.
            if let Err(msg) = paths::assert_nitro_bootstrap() {
                eprintln!("skipping nitro framework test {test_stem}: {msg}");
                return;
            }
            let _lock = crate::lock_fixture();
            // `react-native-nitro-modules` resolves natively from
            // `node_modules`; only a `UBRN_NITRO_LOCAL` override needs a Metro
            // `extraNodeModules` + TSC `paths` entry.
            let nitro_override = paths::nitro_local_override();
            let extras: Vec<(&str, &camino::Utf8Path)> = match nitro_override {
                Some(ref pkg) => vec![("react-native-nitro-modules", pkg.as_path())],
                None => Vec::new(),
            };
            let bundle =
                typescript::prepare_for_jsi_with_extras(test_script, &out_dir, None, &extras);
            run_test_runner_nitro_no_lib(&bundle);
        }
    }
}

fn run_test_runner_nitro_no_lib(bundle: &Utf8Path) {
    let runner = paths::nitro_test_runner_binary();
    let mut cmd = Command::new(runner.as_str());
    cmd.arg(bundle.as_str());
    paths::add_nitro_dll_paths(&mut cmd);
    run_cmd_quietly(&mut cmd);
}

fn run_test_runner_no_lib(bundle: &Utf8Path) {
    let runner = paths::test_runner_binary();
    let mut cmd = Command::new(runner.as_str());
    cmd.arg(bundle.as_str());
    paths::add_hermes_dll_paths(&mut cmd);
    run_cmd_quietly(&mut cmd);
}
