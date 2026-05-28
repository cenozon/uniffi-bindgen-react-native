/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
use std::process::Command;

use anyhow::{anyhow, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Args;
use ubrn_common::{mk_dir, rm_dir, run_cmd};

use crate::{
    bootstrap::HermesCmd,
    util::{build_root, cpp_modules, repository_root},
};

use super::Bootstrap;

/// Local checkout of the user's in-progress Nitro work, used in preference
/// to a fresh clone when it exists. Mirrors the path described in the
/// xtask docs.
const LOCAL_NITRO_PATH: &str = "/home/agent-grant/dev/nitro";

#[derive(Debug, Args)]
pub(crate) struct NitroCmd {
    /// Fetch nitro from this GitHub repo (used only if no local checkout is found).
    #[clap(long, default_value = "mrousavy/nitro")]
    repo: String,

    /// Fetch this branch from the nitro repo (used only if no local checkout is found).
    #[clap(long, short = 'b', default_value = "main")]
    branch: String,
}

impl Default for NitroCmd {
    fn default() -> Self {
        Self {
            repo: "mrousavy/nitro".to_owned(),
            branch: "main".to_owned(),
        }
    }
}

impl NitroCmd {
    /// Where the nitro source tree lives (either a git clone or a symlink to
    /// a local checkout). Sits alongside hermes in `cpp_modules/`.
    pub fn src_dir() -> Result<Utf8PathBuf> {
        Ok(cpp_modules()?.join("nitro"))
    }

    /// Where the cmake build artefacts (including `libNitroModules.*`) live.
    pub fn build_dir() -> Result<Utf8PathBuf> {
        Ok(build_root()?.join("nitro-build"))
    }

    /// Public include root containing a flat `NitroModules/` directory
    /// whose entries symlink (Unix) / copy (Windows) every `.hpp` under
    /// the Nitro cpp/ source tree. Consumer code can then resolve
    /// `#include <NitroModules/Foo.hpp>` directly — the flat-dir
    /// convention is what mobile autolinking would otherwise stage.
    pub(crate) fn include_dir() -> Result<Utf8PathBuf> {
        Ok(Self::build_dir()?.join("include"))
    }

    /// Mirror every `.hpp` under `cpp_src` into a flat
    /// `<build_dir>/include/NitroModules/` directory, so consumers can
    /// `#include <NitroModules/HybridObject.hpp>` (and friends) without
    /// having to know the nested subdirectory each header actually lives in.
    fn populate_flat_includes() -> Result<()> {
        let flat_dir = Self::include_dir()?.join("NitroModules");
        // Start fresh each bootstrap so a deleted upstream header doesn't
        // leave a stale symlink/copy behind to confuse cmake.
        if flat_dir.exists() {
            rm_dir(&flat_dir)?;
        }
        mk_dir(&flat_dir)?;

        let cpp_src = Self::cpp_src_dir()?;
        if !cpp_src.exists() {
            return Err(anyhow!(
                "Nitro cpp source dir {cpp_src} missing; cannot populate flat NitroModules/ include dir"
            ));
        }

        let mut entries = Vec::new();
        collect_hpp_files(cpp_src.as_std_path(), &mut entries)?;
        for src in entries {
            let file_name = src
                .file_name()
                .ok_or_else(|| anyhow!("header {} has no file name", src.display()))?;
            let dst = flat_dir.as_std_path().join(file_name);
            mirror_header(&src, &dst)?;
        }
        Ok(())
    }

    /// The linkable library the desktop test-runner needs.
    /// File extension differs per-platform; mirrors `TestRunnerCmd::exe`.
    pub fn lib_path() -> Result<Utf8PathBuf> {
        let dir = Self::build_dir()?;
        if cfg!(target_os = "windows") {
            // On Windows multi-config generators emit into Debug/.
            Ok(dir.join("Debug").join("NitroModules.dll"))
        } else if cfg!(target_os = "macos") {
            Ok(dir.join("libNitroModules.dylib"))
        } else {
            Ok(dir.join("libNitroModules.so"))
        }
    }

    /// Path to the cpp sources inside whatever src_dir resolves to.
    fn cpp_src_dir() -> Result<Utf8PathBuf> {
        Ok(Self::src_dir()?
            .join("packages")
            .join("react-native-nitro-modules")
            .join("cpp"))
    }

    /// Populate `src_dir`. Precedence:
    ///   1. If `src_dir` already exists, do nothing.
    ///   2. Else, if `LOCAL_NITRO_PATH` exists, symlink it to `src_dir`
    ///      (avoids a redundant clone of in-progress local work).
    ///   3. Else, `git clone -b <branch> --depth 1 https://github.com/<repo>.git`.
    fn checkout(&self) -> Result<()> {
        let dir = Self::src_dir()?;
        if dir.exists() {
            return Ok(());
        }
        let parent = dir.parent().expect("Nitro src dir has no parent");
        ubrn_common::mk_dir(parent)?;

        let local = Utf8Path::new(LOCAL_NITRO_PATH);
        if local.exists() {
            symlink_dir(local, &dir)?;
            return Ok(());
        }

        let repo = format!("https://github.com/{}.git", self.repo);
        run_cmd(
            Command::new("git")
                .arg("clone")
                .arg("--single-branch")
                .arg("--depth")
                .arg("1")
                .arg("-b")
                .arg(self.branch.as_str())
                .arg(&repo)
                .arg(&dir),
        )?;

        Ok(())
    }

    /// Write a CMakeLists.txt into `build_dir`. We deliberately do *not* drop
    /// it into `src_dir` because src_dir may be a symlink into the user's
    /// working tree (see `checkout`). Keeping the cmake file in the build
    /// tree avoids polluting that external repo.
    fn write_cmake_lists(&self) -> Result<Utf8PathBuf> {
        let build_dir = Self::build_dir()?;
        ubrn_common::mk_dir(&build_dir)?;

        let cpp_src = Self::cpp_src_dir()?;
        let hermes_src = HermesCmd::src_dir()?;
        let hermes_build = HermesCmd::build_dir()?;
        let stubs_dir = repository_root()?.join("cpp").join("stubs");
        let platform_desktop_dir = repository_root()?
            .join("runtimes")
            .join("nitro")
            .join("cpp")
            .join("platform-desktop");

        if !cpp_src.exists() {
            return Err(anyhow!(
                "Nitro cpp directory not found at {cpp_src}; was src_dir populated?"
            ));
        }

        let cmake = format!(
            r#"# Auto-generated by xtask bootstrap nitro. Do not edit by hand.
cmake_minimum_required(VERSION 3.22)
project(NitroModules CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

set(NITRO_CPP_DIR "{cpp_src}")
set(HERMES_SRC_DIR "{hermes_src}")
set(HERMES_BUILD_DIR "{hermes_build}")
set(STUBS_DIR "{stubs_dir}")

if (CMAKE_CONFIGURATION_TYPES)
    set(HERMES_CONFIG_SUBDIR "/Debug")
else ()
    set(HERMES_CONFIG_SUBDIR "")
endif ()

# Recursively glob every .cpp under nitro's cpp tree.
file(GLOB_RECURSE NITRO_SOURCES CONFIGURE_DEPENDS "${{NITRO_CPP_DIR}}/*.cpp")
if (NOT NITRO_SOURCES)
    message(FATAL_ERROR "No .cpp sources found under ${{NITRO_CPP_DIR}}")
endif ()

# Nitro's headers cross-reference each other with bare quoted includes
# (e.g. core/HybridObject.hpp does `#include "HybridObjectPrototype.hpp"`
# which actually lives under prototype/). Add every immediate subdir of
# the cpp tree to the include path so the bare includes resolve.
file(GLOB NITRO_INCLUDE_SUBDIRS LIST_DIRECTORIES true RELATIVE "${{NITRO_CPP_DIR}}" "${{NITRO_CPP_DIR}}/*")
set(NITRO_INCLUDE_DIRS "${{NITRO_CPP_DIR}}")
foreach (sub ${{NITRO_INCLUDE_SUBDIRS}})
    if (IS_DIRECTORY "${{NITRO_CPP_DIR}}/${{sub}}")
        list(APPEND NITRO_INCLUDE_DIRS "${{NITRO_CPP_DIR}}/${{sub}}")
    endif ()
endforeach ()

# `cpp/platform/ThreadUtils.hpp` declares static methods whose impls live
# in the per-platform tree (android/ios). For desktop bootstrap we
# substitute a stub from `runtimes/nitro/cpp/platform-desktop/` that
# implements the same surface with std::thread + a synchronous
# InlineDispatcher. Without this libNitroModules fails to link on
# `ThreadUtils::createUIThreadDispatcher`, `isUIThread`, `getThreadName`,
# `setThreadName`.
set(DESKTOP_PLATFORM_DIR "{platform_desktop_dir}")
list(APPEND NITRO_SOURCES "${{DESKTOP_PLATFORM_DIR}}/ThreadUtils.cpp")
list(APPEND NITRO_INCLUDE_DIRS "${{DESKTOP_PLATFORM_DIR}}")

add_library(NitroModules SHARED ${{NITRO_SOURCES}})

target_include_directories(NitroModules PUBLIC
    ${{NITRO_INCLUDE_DIRS}}
    "${{HERMES_SRC_DIR}}/API"
    "${{HERMES_SRC_DIR}}/API/jsi"
    "${{HERMES_SRC_DIR}}/public"
    "${{STUBS_DIR}}"
)

# Link against Hermes' JSI. find_library handles per-platform naming.
find_library(JSI_LIB NAMES jsi
    PATHS "${{HERMES_BUILD_DIR}}/jsi${{HERMES_CONFIG_SUBDIR}}"
    NO_DEFAULT_PATH)
if (NOT JSI_LIB)
    message(FATAL_ERROR "Could not find jsi library under ${{HERMES_BUILD_DIR}}/jsi")
endif ()
target_link_libraries(NitroModules PUBLIC ${{JSI_LIB}})

if (MSVC)
    target_compile_definitions(NitroModules PRIVATE NOMINMAX)
endif ()

# Workaround: NitroTypeInfo.cpp uses std::unordered_map but does not
# include the header. It transitively comes in on libc++ (Apple) but not
# on libstdc++ (Linux). Force-include the header on the affected TU.
if (NOT MSVC)
    set_source_files_properties(
        "${{NITRO_CPP_DIR}}/utils/NitroTypeInfo.cpp"
        PROPERTIES COMPILE_FLAGS "-include unordered_map"
    )
endif ()
"#
        );

        let path = build_dir.join("CMakeLists.txt");
        ubrn_common::write_file(&path, cmake)?;
        Ok(path)
    }
}

impl Bootstrap for NitroCmd {
    /// The built library is the bootstrap-completed marker — same convention
    /// as `TestRunnerCmd` (which uses the built executable).
    fn marker() -> Result<Utf8PathBuf> {
        Self::lib_path()
    }

    fn clean() -> Result<()> {
        rm_dir(Self::build_dir()?)?;
        // src_dir may be a symlink into the user's working tree; rm_dir on a
        // symlink-to-dir removes only the link, not the target. Safe either way.
        rm_dir(Self::src_dir()?)?;
        Ok(())
    }

    fn prepare(&self) -> Result<()> {
        // Hermes is a hard prerequisite — we link against its libjsi and need
        // its headers.
        HermesCmd::default().ensure_ready()?;

        self.checkout()?;
        let _ = self.write_cmake_lists()?;

        let build_dir = Self::build_dir()?;

        let mut cmd = Command::new("cmake");
        cmd.current_dir(&build_dir)
            .arg("-G")
            .arg(if cfg!(target_os = "windows") {
                "Visual Studio 16 2019"
            } else {
                "Ninja"
            })
            .arg("-DCMAKE_BUILD_TYPE=Release")
            .arg(&build_dir);
        run_cmd(&mut cmd)?;

        if cfg!(target_os = "windows") {
            let mut cmd = Command::new("cmake");
            run_cmd(cmd.current_dir(&build_dir).arg("--build").arg(&build_dir))?;
        } else {
            let mut cmd = Command::new("ninja");
            run_cmd(cmd.current_dir(&build_dir))?;
        }

        // After libNitroModules.{so,dylib,dll} is built, mirror every
        // upstream `.hpp` into a flat `<build_dir>/include/NitroModules/`
        // directory. Consumer code references Nitro headers via
        // `<NitroModules/Foo.hpp>` (the convention the npm autolinking
        // sets up on mobile); the flat dir is what downstream cmake
        // include paths point at on desktop.
        Self::populate_flat_includes()?;

        Ok(())
    }
}

/// Recursively collect every `.hpp` file under `dir`. Used to build the
/// flat `NitroModules/` include directory mirror.
fn collect_hpp_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .map_err(|e| anyhow!("read_dir {}: {e}", dir.display()))?
    {
        let entry = entry.map_err(|e| anyhow!("read_dir entry under {}: {e}", dir.display()))?;
        let path = entry.path();
        let ft = entry
            .file_type()
            .map_err(|e| anyhow!("file_type {}: {e}", path.display()))?;
        if ft.is_dir() {
            collect_hpp_files(&path, out)?;
        } else if ft.is_file()
            && path.extension().and_then(|e| e.to_str()) == Some("hpp")
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Create the `dst` mirror of `src`. Symlink on Unix, copy on Windows.
fn mirror_header(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if dst.exists() {
        std::fs::remove_file(dst)
            .map_err(|e| anyhow!("failed to remove stale {}: {e}", dst.display()))?;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dst).map_err(|e| {
            anyhow!(
                "failed to symlink {} -> {}: {e}",
                src.display(),
                dst.display()
            )
        })?;
    }
    #[cfg(windows)]
    {
        std::fs::copy(src, dst).map_err(|e| {
            anyhow!(
                "failed to copy {} -> {}: {e}",
                src.display(),
                dst.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_dir(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    std::os::unix::fs::symlink(src.as_std_path(), dst.as_std_path())
        .map_err(|e| anyhow!("failed to symlink {src} -> {dst}: {e}"))
}

#[cfg(windows)]
fn symlink_dir(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(src.as_std_path(), dst.as_std_path())
        .map_err(|e| anyhow!("failed to symlink {src} -> {dst}: {e}"))
}
