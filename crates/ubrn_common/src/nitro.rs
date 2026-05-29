/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
//! Resolution of the `react-native-nitro-modules` package the Nitro backend
//! builds and bundles against.
//!
//! Nitro is consumed exactly the way a real React Native app consumes it: as
//! the published npm package installed into `node_modules`. The package ships
//! both the TS runtime (for Metro/tsc) and the full C++ source tree under
//! `cpp/` (for building `libNitroModules` and the test runner on the host), so
//! no separate checkout of the Nitro monorepo is required.
//!
//! A single opt-in override, [`LOCAL_ENV`], lets a developer point the whole
//! toolchain at a local Nitro package while iterating on unreleased changes.
//! It is resolved to its real on-disk location so that nothing downstream
//! (Metro's realpath-keyed module graph in particular) depends on a symlink
//! surviving. Both the `xtask` build steps and the fixture-test driver resolve
//! through [`package_dir`], so the override is honored consistently.

use anyhow::{anyhow, Result};
use camino::{Utf8Path, Utf8PathBuf};

/// Environment variable pointing at a local `react-native-nitro-modules`
/// package directory (the one containing `package.json` and `cpp/`), used in
/// preference to the copy in `node_modules`. Intended for iterating on
/// unreleased Nitro changes; left unset in CI.
pub const LOCAL_ENV: &str = "UBRN_NITRO_LOCAL";

/// The npm package name, and its directory name under `node_modules`.
pub const PACKAGE_NAME: &str = "react-native-nitro-modules";

/// Resolve the [`LOCAL_ENV`] override, if set.
///
/// Returns `Ok(None)` when the variable is unset or empty. When set, the path
/// is canonicalized (resolving symlinks and `..`) and validated to actually be
/// a Nitro package; a set-but-unusable value is a hard error rather than a
/// silent fall-through to `node_modules`, so a typo'd override never masks
/// itself by testing the wrong Nitro.
pub fn local_override() -> Result<Option<Utf8PathBuf>> {
    let raw = match std::env::var(LOCAL_ENV) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Ok(None),
    };
    let real = dunce::canonicalize(&raw)
        .map_err(|e| anyhow!("{LOCAL_ENV}={raw:?} could not be resolved: {e}"))?;
    let dir = Utf8PathBuf::from_path_buf(real)
        .map_err(|p| anyhow!("{LOCAL_ENV} resolves to a non-UTF-8 path: {}", p.display()))?;
    validate_package(&dir, &format!("{LOCAL_ENV}={raw:?}"))?;
    Ok(Some(dir))
}

/// Resolve the `react-native-nitro-modules` package directory: the [`LOCAL_ENV`]
/// override if set, otherwise the copy installed under `<repo_root>/node_modules`.
///
/// The returned directory contains the package's `package.json`, its TS
/// (`lib/`, `src/`) and the C++ source tree (`cpp/`).
pub fn package_dir(repo_root: &Utf8Path) -> Result<Utf8PathBuf> {
    if let Some(local) = local_override()? {
        return Ok(local);
    }
    let dir = repo_root.join("node_modules").join(PACKAGE_NAME);
    validate_package(
        &dir,
        &format!(
            "{PACKAGE_NAME} is not installed under node_modules — run `yarn install` \
             (or set {LOCAL_ENV} to a local Nitro package)"
        ),
    )?;
    Ok(dir)
}

/// The C++ source root inside a resolved Nitro package — the include/compile
/// root for `libNitroModules`, the host test runner, and per-fixture glue.
pub fn cpp_dir(repo_root: &Utf8Path) -> Result<Utf8PathBuf> {
    let pkg = package_dir(repo_root)?;
    let cpp = pkg.join("cpp");
    if !cpp.join("entrypoint").join("InstallNitro.hpp").exists() {
        return Err(anyhow!(
            "{pkg} does not contain cpp/entrypoint/InstallNitro.hpp — is this a \
             react-native-nitro-modules package?"
        ));
    }
    Ok(cpp)
}

fn validate_package(dir: &Utf8Path, ctx: &str) -> Result<()> {
    if !dir.join("package.json").exists() {
        return Err(anyhow!("{ctx}: no package.json found at {dir}"));
    }
    Ok(())
}
