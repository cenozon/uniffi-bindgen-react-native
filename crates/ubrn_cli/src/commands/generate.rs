/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */

use std::convert::TryFrom;
use std::process::Command;

use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{self, Args, Subcommand};

use ubrn_bindgen::{AbiFlavor, BindingsArgs, BindingsOutcome, SwitchArgs};

#[cfg(feature = "wasm")]
use crate::wasm;
use crate::{
    codegen::{files, get_template_config, render_files},
    config::{Framework, ProjectConfig},
    jsi, napi, Platform,
};

use super::ConfigArgs;

#[derive(Args, Debug)]
pub(crate) struct GenerateArgs {
    #[clap(subcommand)]
    cmd: GenerateCmd,
}

impl GenerateArgs {
    pub(crate) fn run(&self) -> Result<()> {
        self.cmd.run()
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum GenerateCmd {
    /// Commands which re-direct to the jsi version.
    ///
    /// These are now deprecated and so hidden.
    #[clap(hide = true)]
    Bindings(BindingsArgs),
    #[clap(hide = true)]
    TurboModule(jsi::TurboModuleArgs),
    #[clap(hide = true)]
    All(GenerateAllArgs),

    /// Commands to generate the JSI bindings and turbo-module code.
    #[clap(aliases = ["react-native", "rn"])]
    Jsi(jsi::CmdArg),

    /// Commands to generate N-API (Node.js) bindings.
    #[clap(aliases = ["node"])]
    Napi(napi::CmdArg),

    /// Commands to generate a WASM crate.
    #[cfg(feature = "wasm")]
    #[clap(aliases = ["web"])]
    Wasm(wasm::CmdArg),
}

impl GenerateCmd {
    pub(crate) fn run(&self) -> Result<()> {
        match self {
            Self::Bindings(b) => {
                b.run(None)?;
                Ok(())
            }
            Self::TurboModule(t) => {
                t.run()?;
                Ok(())
            }
            Self::All(t) => {
                let t = GenerateAllCommand::try_from(t)?;
                t.run()?;
                Ok(())
            }
            Self::Jsi(jsi) => {
                jsi.run()?;
                Ok(())
            }
            Self::Napi(napi) => {
                napi.run()?;
                Ok(())
            }
            #[cfg(feature = "wasm")]
            Self::Wasm(wasm) => {
                wasm.run()?;
                Ok(())
            }
        }
    }
}

#[derive(Args, Debug)]
pub(crate) struct GenerateAllArgs {
    #[clap(flatten)]
    config: ConfigArgs,

    #[cfg(feature = "wasm")]
    #[command(flatten)]
    switches: SwitchArgs,

    /// A path to staticlib file.
    lib_file: Utf8PathBuf,

    /// Whether to generate native bindings or not.
    #[clap(long, default_value = "false")]
    native_bindings: bool,
}

#[derive(Debug)]
pub(crate) struct GenerateAllCommand {
    /// The configuration file for this project
    project_config: ProjectConfig,

    /// A path to staticlib file.
    lib_file: Utf8PathBuf,

    platform: Option<Platform>,

    /// Whether to generate native bindings or not.
    native_bindings: bool,
}

impl GenerateAllCommand {
    pub(crate) fn new(
        lib_file: Utf8PathBuf,
        project_config: ProjectConfig,
        native_bindings: bool,
    ) -> Self {
        Self {
            lib_file,
            project_config,
            platform: None,
            native_bindings,
        }
    }

    pub(crate) fn platform_specific(
        lib_file: Utf8PathBuf,
        project_config: ProjectConfig,
        platform: Platform,
        native_bindings: bool,
    ) -> Self {
        Self {
            lib_file,
            project_config,
            platform: Some(platform),
            native_bindings,
        }
    }

    fn switches(&self) -> SwitchArgs {
        // Framework choice wins over platform: `framework: nitro` always
        // selects `AbiFlavor::Nitro` regardless of Android/iOS target.
        // Otherwise fall back to the legacy platform-derived flavor.
        let flavor = if matches!(self.project_config.framework, Framework::Nitro) {
            AbiFlavor::Nitro
        } else {
            self.platform.as_ref().map_or(AbiFlavor::Jsi, |p| p.into())
        };
        SwitchArgs { flavor }
    }

    pub(crate) fn run(&self) -> Result<()> {
        let pwd = ubrn_common::pwd()?;
        let lib_file = pwd.join(&self.lib_file);
        let native_bindings = self.native_bindings;

        // Step 1: Generate bindings
        let outcome = self.generate_bindings(&lib_file)?;

        // Step 2: Generate template files
        self.generate_template_files(outcome, native_bindings)?;

        Ok(())
    }

    fn generate_bindings(&self, lib_file: &Utf8PathBuf) -> Result<BindingsOutcome> {
        let project = &self.project_config;
        let switches = self.switches();
        let pwd = ubrn_common::pwd()?;
        let bindings = self.create_bindings_command(lib_file, project, switches)?;

        ubrn_common::cd(&project.crate_.crate_dir()?)?;
        let manifest_path = project.crate_.manifest_path()?;
        let outcome = bindings.run(Some(&manifest_path))?;
        ubrn_common::cd(&pwd)?;

        Ok(outcome)
    }

    fn create_bindings_command(
        &self,
        lib_file: &Utf8PathBuf,
        project: &ProjectConfig,
        switches: SwitchArgs,
    ) -> Result<BindingsArgs, anyhow::Error> {
        Ok(match self.platform {
            #[cfg(feature = "wasm")]
            Some(Platform::Wasm) => wasm::bindings(project, switches, lib_file)?,
            _ => jsi::bindings(project, switches, lib_file)?,
        })
    }

    fn generate_template_files(
        &self,
        outcome: BindingsOutcome,
        native_bindings: bool,
    ) -> Result<()> {
        let project = &self.project_config;
        let rust_crate = project.crate_.metadata()?;
        let BindingsOutcome {
            modules,
            nitro_hybrid_objects,
        } = outcome;
        let config = get_template_config(
            project.clone(),
            rust_crate,
            modules,
            nitro_hybrid_objects,
            native_bindings,
        );

        let files = match &self.platform {
            Some(platform) => files::get_files_for(config.clone(), platform),
            None => files::get_files(config.clone()),
        };

        render_files(config.clone(), files.into_iter())?;

        if matches!(project.framework, Framework::Nitro) {
            run_nitrogen(project)?;
        }

        Ok(())
    }
}

/// Drive Nitrogen as a subprocess immediately after ubrn emits its Nitro
/// shell. Nitrogen consumes the `.nitro.ts` spec we just wrote and produces
/// the autolinking glue (`HybridXxxSpec.hpp/.cpp`, `*OnLoad.cpp`,
/// `+autolinking.{rb,gradle,cmake}`) that our gradle/cmake/podspec templates
/// reference. If Nitrogen isn't installed in the project's node_modules we
/// surface a clear message rather than failing silently — the user can then
/// run `bun add -d nitrogen` and rerun.
pub(crate) fn run_nitrogen(project: &ProjectConfig) -> Result<()> {
    let project_root = project.project_root();
    let nitrogen_entry = project_root.join("node_modules/nitrogen/lib/index.js");
    if !nitrogen_entry.exists() {
        eprintln!(
            "warning: nitrogen not found at {nitrogen_entry}; \
             install it (e.g. `bun add -d nitrogen react-native-nitro-modules`) \
             and re-run to finish wiring up the Nitro autolinking outputs."
        );
        return Ok(());
    }

    // Prefer `bun` if available; fall back to `node`. The user has explicitly
    // chosen Nitro, so they likely already have a JS runtime in $PATH.
    let runtime = if Command::new("bun").arg("--version").output().is_ok() {
        "bun"
    } else {
        "node"
    };

    let status = Command::new(runtime)
        .arg(nitrogen_entry.as_str())
        .current_dir(project_root)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn `{runtime} nitrogen`: {e}"))?;

    if !status.success() {
        anyhow::bail!(
            "nitrogen exited with status {status}; \
             inspect the output above for details"
        );
    }

    Ok(())
}

impl TryFrom<&GenerateAllArgs> for GenerateAllCommand {
    type Error = anyhow::Error;

    fn try_from(value: &GenerateAllArgs) -> Result<Self> {
        let project_config = value.config.clone().try_into()?;
        let native_bindings = value.native_bindings;
        Ok(GenerateAllCommand::new(
            value.lib_file.clone(),
            project_config,
            native_bindings,
        ))
    }
}
