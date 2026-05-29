/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use clap::Args;
use serde::Deserialize;
use ubrn_common::{mk_dir, path_or_shim, CrateMetadata, Utf8PathBufExt as _};
use uniffi_bindgen::{
    cargo_metadata::CrateConfigSupplier, pipeline::general, BindgenLoader, BindgenPaths,
    ComponentInterface,
};

#[cfg(feature = "wasm")]
use super::{bindings::gen_rust, wasm::generate_rs};
use super::{
    bindings::{
        gen_cpp,
        gen_nitro::{self, HybridObjectEntry},
        gen_typescript,
        metadata::ModuleMetadata,
    },
    react_native::generate_cpp,
    switches::{AbiFlavor, SwitchArgs},
};

/// Aggregated result of a `BindingsArgs::run` invocation.
///
/// Carries the per-namespace [`ModuleMetadata`] every flavor produces,
/// plus the Nitro-specific [`HybridObjectEntry`] list that the
/// `gen_nitro` backend emits. Non-Nitro flavors leave
/// `nitro_hybrid_objects` empty; the platform-glue layer in `ubrn_cli`
/// uses it to populate `nitro.json#autolinking` and to enumerate the
/// `Hybrid*.cpp` source list in the Android `CMakeLists.txt`.
///
/// The struct is intentionally non-exhaustive — adding future per-flavor
/// metadata (wasm exports, napi entrypoints, …) should be a non-breaking
/// change.
#[derive(Default, Debug)]
pub struct BindingsOutcome {
    pub modules: Vec<ModuleMetadata>,
    pub nitro_hybrid_objects: Vec<HybridObjectEntry>,
}

#[derive(Args, Debug)]
pub struct BindingsArgs {
    #[command(flatten)]
    pub(crate) source: SourceArgs,
    #[command(flatten)]
    pub(crate) output: OutputArgs,
    /// Flavor of bindings to emit. Used to dispatch between the legacy
    /// JSI host-object backend, the napi player backend, the Nitro
    /// HybridObject backend, and the wasm-bindgen backend.
    #[command(flatten)]
    switches: SwitchArgs,

    /// Set by the napi CLI; required for the `Napi` flavor (other flavors ignore it).
    #[clap(skip)]
    pub(crate) lib_resolution: Option<gen_typescript::ffi_module_player::LibResolution>,
}

impl BindingsArgs {
    pub fn new(switches: SwitchArgs, source: SourceArgs, output: OutputArgs) -> Self {
        Self {
            switches,
            source,
            output,
            lib_resolution: None,
        }
    }

    pub fn with_lib_resolution(
        mut self,
        resolution: gen_typescript::ffi_module_player::LibResolution,
    ) -> Self {
        self.lib_resolution = Some(resolution);
        self
    }

    pub fn ts_dir(&self) -> &Utf8Path {
        &self.output.ts_dir
    }

    pub fn cpp_dir(&self) -> &Utf8Path {
        &self.output.cpp_dir
    }

    pub fn switches(&self) -> SwitchArgs {
        self.switches.clone()
    }
}

#[derive(Args, Clone, Debug)]
pub struct OutputArgs {
    /// By default, bindgen will attempt to format the code with prettier and clang-format.
    #[clap(long)]
    pub(crate) no_format: bool,

    /// The directory in which to put the generated Typescript.
    #[clap(long)]
    pub(crate) ts_dir: Utf8PathBuf,

    /// The directory in which to put the generated C++.
    #[clap(long, alias = "abi-dir")]
    pub(crate) cpp_dir: Utf8PathBuf,

    /// Append this extension to every relative import specifier in the
    /// generated TypeScript (e.g. `--import-extension js` produces
    /// `from './foo.js'`). Unset = today's behavior (`from './foo'`).
    ///
    /// Node ESM via `tsc --module nodenext` requires explicit extensions
    /// (`tsc` preserves specifiers verbatim into emitted `.js`, and Node
    /// rejects extensionless ones with `ERR_MODULE_NOT_FOUND`). Bundlers
    /// (metro, webpack, rollup, esbuild) accept either form, so leaving
    /// this off keeps bundler-fed consumers byte-identical.
    ///
    /// A leading `.` is tolerated (`.js` and `js` are equivalent). Most
    /// users want `js`; `mjs`/`cjs` are reserved for future generators
    /// that emit those filenames directly.
    #[clap(long, value_name = "EXT")]
    pub(crate) import_extension: Option<String>,
}

impl OutputArgs {
    pub fn new(ts_dir: &Utf8Path, cpp_dir: &Utf8Path, no_format: bool) -> Self {
        Self {
            ts_dir: ts_dir.to_owned(),
            cpp_dir: cpp_dir.to_owned(),
            no_format,
            import_extension: None,
        }
    }

    /// Builder-style setter for the import-extension option. Mirrors the
    /// CLI flag for callers that construct `OutputArgs` programmatically
    /// (e.g. the wrapping `ubrn_cli` crate).
    pub fn with_import_extension(mut self, ext: Option<String>) -> Self {
        self.import_extension = ext;
        self
    }
}

#[derive(Args, Clone, Debug, Default)]
pub struct SourceArgs {
    /// The path to a dynamic library to attempt to extract the definitions from
    /// and extend the component interface with.
    #[clap(long)]
    pub(crate) lib_file: Option<Utf8PathBuf>,

    /// Override the default crate name that is guessed from UDL file path.
    #[clap(long = "crate")]
    pub(crate) crate_name: Option<String>,

    /// The location of the uniffi.toml file
    #[clap(long)]
    pub(crate) config: Option<Utf8PathBuf>,

    /// Treat the input file as a library, extracting any Uniffi definitions from that.
    #[clap(long = "library", conflicts_with_all = ["config", "lib_file"])]
    pub(crate) library_mode: bool,

    /// A UDL file or library file
    pub(crate) source: Utf8PathBuf,
}

impl SourceArgs {
    pub fn library(file: &Utf8PathBuf) -> Self {
        Self {
            library_mode: true,
            source: file.clone(),
            ..Default::default()
        }
    }

    /// Returns the source path (a UDL file or library file).
    pub fn source(&self) -> &Utf8PathBuf {
        &self.source
    }

    pub fn with_config(self, config: Option<Utf8PathBuf>) -> Self {
        Self {
            config,
            library_mode: self.library_mode,
            source: self.source,
            lib_file: self.lib_file,
            crate_name: self.crate_name,
        }
    }
}

impl BindingsArgs {
    pub fn run(&self, manifest_path: Option<&Utf8PathBuf>) -> Result<BindingsOutcome> {
        let out = &self.output;

        mk_dir(&out.ts_dir)?;
        mk_dir(&out.cpp_dir)?;
        let ts_dir = out.ts_dir.canonicalize_utf8_or_shim()?;
        let abi_dir = out.cpp_dir.canonicalize_utf8_or_shim()?;
        let switches = self.switches();

        let source_path = path_or_shim(&self.source.source)?;
        let loader = self.create_loader(manifest_path)?;

        // C++/Rust generation via ComponentInterface
        match &switches.flavor {
            AbiFlavor::Jsi => {
                let metadata = loader.load_metadata(&source_path)?;
                let cis = loader.load_cis(metadata)?;
                let mut components = loader.load_components(cis, parse_cpp_config)?;
                for c in components.iter_mut() {
                    c.ci.derive_ffi_funcs()?;
                }
                generate_cpp(&components, &abi_dir, !out.no_format)?;
            }
            AbiFlavor::Napi => { /* No C++ generation for Napi */ }
            // Nitro emits its C++ exclusively via the pipeline-driven
            // `gen_nitro::generate_all` below — no ComponentInterface
            // path needed.
            AbiFlavor::Nitro => { /* handled below */ }
            #[cfg(feature = "wasm")]
            AbiFlavor::Wasm => {
                let metadata = loader.load_metadata(&source_path)?;
                let cis = loader.load_cis(metadata)?;
                let mut components = loader.load_components(cis, parse_rust_config)?;
                for c in components.iter_mut() {
                    c.ci.derive_ffi_funcs()?;
                }
                generate_rs(&components, &switches, &abi_dir, !out.no_format)?;
            }
        }

        // TypeScript generation via pipeline
        // The pipeline needs per-crate configs (not the --config override) so that
        // each namespace gets its own crate's uniffi.toml (e.g. custom type mappings).
        // TODO check this is the desired behavior in uniffi-rs 0.31.x.
        let pipeline_loader = self.create_pipeline_loader(manifest_path)?;
        let metadata = pipeline_loader.load_metadata(&source_path)?;
        let initial_root = pipeline_loader.load_pipeline_initial_root(&source_path, metadata)?;
        let general_root = general::pipeline("react-native").execute(initial_root)?;

        // Nitro is a fully separate codegen — both TS and C++ come from
        // `gen_nitro` and we skip the legacy TS+JSI-cpp emission entirely.
        // The returned `nitro_hybrid_objects` flows out through
        // `BindingsOutcome` into the platform-glue layer, which uses it
        // to populate `nitro.json#autolinking` and the Android
        // `CMakeLists.txt` source list.
        if switches.flavor == AbiFlavor::Nitro {
            let emission = gen_nitro::generate_all(&general_root, &ts_dir, &abi_dir)?;
            return Ok(BindingsOutcome {
                modules: emission.modules,
                nitro_hybrid_objects: emission.hybrid_objects,
            });
        }

        generate_ffi_from_pipeline(
            &general_root,
            &switches,
            &ts_dir,
            self.lib_resolution.clone(),
            out.import_extension.as_deref(),
        )?;
        let modules = generate_api_from_pipeline(
            &general_root,
            &switches,
            &ts_dir,
            out.import_extension.as_deref(),
        )?;
        if switches.flavor == AbiFlavor::Napi {
            generate_index_from_modules(
                &modules,
                &switches,
                &ts_dir,
                out.import_extension.as_deref(),
            )?;
        }
        if !out.no_format {
            gen_typescript::format_directory(&ts_dir)?;
        }
        Ok(BindingsOutcome {
            modules,
            nitro_hybrid_objects: Vec::new(),
        })
    }

    fn create_loader(&self, manifest_path: Option<&Utf8PathBuf>) -> Result<BindgenLoader> {
        let mut bindgen_paths = BindgenPaths::default();
        if let Some(config_path) = &self.source.config {
            bindgen_paths.add_config_override_layer(config_path.clone());
        }
        let cwd = Utf8PathBuf::from("Cargo.toml");
        let manifest_path = manifest_path.unwrap_or(&cwd);
        let cargo_metadata = CrateMetadata::cargo_metadata(manifest_path)?;
        let config_supplier = CrateConfigSupplier::from(cargo_metadata);
        bindgen_paths.add_layer(config_supplier);
        Ok(BindgenLoader::new(bindgen_paths))
    }

    /// Create a loader for the pipeline that uses only per-crate configs.
    ///
    /// The `--config` override applies a single TOML to ALL crates, which breaks
    /// multi-crate scenarios where each dependency has its own `uniffi.toml`
    /// (e.g. custom type mappings). The pipeline needs each namespace to get its
    /// own crate's config.
    fn create_pipeline_loader(&self, manifest_path: Option<&Utf8PathBuf>) -> Result<BindgenLoader> {
        let mut bindgen_paths = BindgenPaths::default();
        let cwd = Utf8PathBuf::from("Cargo.toml");
        let manifest_path = manifest_path.unwrap_or(&cwd);
        let cargo_metadata = CrateMetadata::cargo_metadata(manifest_path)?;
        let config_supplier = CrateConfigSupplier::from(cargo_metadata);
        bindgen_paths.add_layer(config_supplier);
        Ok(BindgenLoader::new(bindgen_paths))
    }
}

fn generate_api_from_pipeline(
    general_root: &general::Root,
    switches: &SwitchArgs,
    ts_dir: &Utf8Path,
    import_extension: Option<&str>,
) -> Result<Vec<ModuleMetadata>> {
    let mut modules = Vec::new();
    for (name, namespace) in &general_root.namespaces {
        let config = extract_ts_config(namespace)?.with_import_extension(import_extension);
        let module = ModuleMetadata::new(name);
        let ffi_module = gen_typescript::ffi_module::TsFfiModule::from_general(
            namespace,
            &switches.flavor,
            &config,
        );
        let ffi_exports = ffi_module.exported_names();
        let api_module = gen_typescript::api_module::TsApiModule::from_general(
            &config,
            namespace,
            switches.flavor.clone(),
            ffi_exports,
        );
        let code = gen_typescript::generate_api_code_from_ir(api_module)?;
        let path = ts_dir.join(module.ts_filename());
        ubrn_common::write_file(path, code)?;
        modules.push(module);
    }
    Ok(modules)
}

fn generate_index_from_modules(
    modules: &[ModuleMetadata],
    switches: &SwitchArgs,
    ts_dir: &Utf8Path,
    import_extension: Option<&str>,
) -> Result<()> {
    let code = gen_typescript::generate_index_code(
        modules.to_vec(),
        switches.flavor.clone(),
        import_extension,
    )?;
    let path = ts_dir.join("index.ts");
    ubrn_common::write_file(path, code)?;
    Ok(())
}

fn extract_ts_config(namespace: &general::Namespace) -> Result<gen_typescript::Config> {
    #[derive(Default, Deserialize)]
    struct BindingsSection {
        #[serde(default, alias = "javascript", alias = "js", alias = "ts")]
        typescript: gen_typescript::Config,
    }
    #[derive(Default, Deserialize)]
    struct ConfigRoot {
        #[serde(default)]
        bindings: BindingsSection,
    }
    let Some(ref config_toml) = namespace.config_toml else {
        return Ok(Default::default());
    };
    let root: ConfigRoot = toml::from_str(config_toml)?;
    Ok(root.bindings.typescript)
}

fn generate_ffi_from_pipeline(
    root: &general::Root,
    switches: &SwitchArgs,
    ts_dir: &Utf8Path,
    lib_resolution: Option<gen_typescript::ffi_module_player::LibResolution>,
    import_extension: Option<&str>,
) -> Result<()> {
    for (name, namespace) in &root.namespaces {
        let module = ModuleMetadata::new(name);
        let path = ts_dir.join(module.ts_ffi_filename());

        let config = extract_ts_config(namespace)?.with_import_extension(import_extension);
        let code = match &switches.flavor {
            AbiFlavor::Napi => {
                let lib_resolution = lib_resolution.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "napi codegen requires a LibResolution; pass --lib-colocated or --lib-absolute"
                    )
                })?;
                let crate_name = namespace.crate_name.clone();
                let player_module =
                    gen_typescript::ffi_module_player::PlayerFfiModule::from_general(
                        namespace,
                        &config,
                        crate_name,
                        lib_resolution,
                    );
                gen_typescript::generate_player_lowlevel_code(player_module)?
            }
            _ => {
                let ffi_module = gen_typescript::ffi_module::TsFfiModule::from_general(
                    namespace,
                    &switches.flavor,
                    &config,
                );
                gen_typescript::generate_lowlevel_code(ffi_module)?
            }
        };

        ubrn_common::write_file(path, code)?;
    }
    Ok(())
}

fn parse_cpp_config(_ci: &ComponentInterface, toml: toml::Value) -> Result<gen_cpp::Config> {
    match toml.get("bindings").and_then(|b| b.get("cpp")) {
        Some(v) => Ok(v.clone().try_into()?),
        None => Ok(Default::default()),
    }
}

#[cfg(feature = "wasm")]
fn parse_rust_config(_ci: &ComponentInterface, toml: toml::Value) -> Result<gen_rust::Config> {
    let value = toml
        .get("bindings")
        .and_then(|b| b.get("rust").or_else(|| b.get("rs")));
    match value {
        Some(v) => Ok(v.clone().try_into()?),
        None => Ok(Default::default()),
    }
}
