/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
use std::process::Command;

use anyhow::Result;
use camino::Utf8PathBuf;
use clap::Args;
use ubrn_common::{rm_dir, run_cmd};

use crate::{
    bootstrap::{HermesCmd, NitroCmd},
    util::{build_root, repository_root},
};

use super::Bootstrap;

#[derive(Debug, Args, Default)]
pub(crate) struct NitroTestRunnerCmd;

impl NitroTestRunnerCmd {
    /// Shared test-harness source dir. Its single CMakeLists defines both
    /// `test-runner` (JSI) and `test-runner-nitro`; we configure with the
    /// NITRO_* vars set and build only the Nitro target (see `prepare`).
    fn src_dir() -> Result<Utf8PathBuf> {
        let root = repository_root()?;
        Ok(root.join("cpp").join("test-harness"))
    }

    fn build_dir() -> Result<Utf8PathBuf> {
        let root = build_root()?;
        Ok(root.join("test-runner-nitro"))
    }

    fn exe() -> Result<Utf8PathBuf> {
        let root = Self::build_dir()?;
        if cfg!(target_os = "windows") {
            Ok(root.join("Debug").join("test-runner-nitro.exe"))
        } else {
            Ok(root.join("test-runner-nitro"))
        }
    }

    /// Path to the directory containing the freshly-built
    /// `libNitroModules.{so,dylib,dll}`. NitroCmd places its build artefacts at
    /// `<build_root>/nitro-build/` (see `NitroCmd::build_dir`).
    fn nitro_lib_dir() -> Result<Utf8PathBuf> {
        NitroCmd::build_dir()
    }

    /// Path to the NitroModules cpp/ source tree, used as the include root.
    fn nitro_include_dir() -> Result<Utf8PathBuf> {
        Ok(NitroCmd::src_dir()?
            .join("packages")
            .join("react-native-nitro-modules")
            .join("cpp"))
    }
}

impl Bootstrap for NitroTestRunnerCmd {
    fn marker() -> Result<Utf8PathBuf> {
        Self::exe()
    }

    fn clean() -> Result<()> {
        rm_dir(Self::build_dir()?)?;
        Ok(())
    }

    fn prepare(&self) -> Result<()> {
        // Make sure all upstream dependencies are built before we configure cmake.
        HermesCmd::default().ensure_ready()?;
        NitroCmd::default().ensure_ready()?;

        let dir = Self::build_dir()?;
        let hermes_src = HermesCmd::src_dir()?;
        let hermes_build = HermesCmd::build_dir()?;
        let nitro_lib_dir = Self::nitro_lib_dir()?;
        let nitro_include_dir = Self::nitro_include_dir()?;
        // Flat `<build>/include/NitroModules/` mirror — required by any
        // source file that uses `<NitroModules/Foo.hpp>`-style includes
        // (the convention npm autolinking sets up on mobile).
        let nitro_flat_include_dir = NitroCmd::include_dir()?;

        let src_dir = NitroTestRunnerCmd::src_dir()?;

        ubrn_common::mk_dir(&dir)?;

        let mut cmd = Command::new("cmake");
        cmd.current_dir(&dir)
            .arg("-G")
            .arg(if cfg!(target_os = "windows") {
                "Visual Studio 16 2019"
            } else {
                "Ninja"
            })
            .arg("-DCMAKE_BUILD_TYPE=Release")
            .arg(format!("-DHERMES_SRC_DIR={hermes_src}"))
            .arg(format!("-DHERMES_BUILD_DIR={hermes_build}"))
            .arg(format!("-DNITRO_INCLUDE_DIR={nitro_include_dir}"))
            .arg(format!("-DNITRO_FLAT_INCLUDE_DIR={nitro_flat_include_dir}"))
            .arg(format!("-DNITRO_LIB_DIR={nitro_lib_dir}"))
            .arg(&src_dir);

        run_cmd(&mut cmd)?;

        // The shared CMakeLists also defines the JSI `test-runner` target;
        // build only the Nitro one here.
        if cfg!(target_os = "windows") {
            let mut cmd = Command::new("cmake");
            run_cmd(
                cmd.current_dir(&dir)
                    .arg("--build")
                    .arg(&dir)
                    .arg("--target")
                    .arg("test-runner-nitro"),
            )?;
        } else {
            let mut cmd = Command::new("ninja");
            run_cmd(cmd.current_dir(&dir).arg("test-runner-nitro"))?;
        }

        Ok(())
    }
}
