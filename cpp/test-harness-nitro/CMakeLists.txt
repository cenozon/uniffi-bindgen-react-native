#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/
#

cmake_minimum_required(VERSION 3.22)
project(test-runner-nitro)

# Nitro requires C++20.
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

set(HERMES_SRC_DIR "" CACHE PATH "Path to Hermes source directory")
set(HERMES_BUILD_DIR "" CACHE PATH "Path to Hermes build directory")
set(NITRO_INCLUDE_DIR "" CACHE PATH "Path to NitroModules cpp/ source root")
set(NITRO_FLAT_INCLUDE_DIR "" CACHE PATH "Path to the directory containing a flat NitroModules/ subdir of every Nitro .hpp")
set(NITRO_LIB_DIR "" CACHE PATH "Path to the directory containing libNitroModules.{so,dylib,dll}")

if (NOT HERMES_SRC_DIR)
    message(FATAL_ERROR "Please specify HERMES_SRC_DIR")
endif ()
# Validate HERMES_SRC_DIR by checking for API/jsi/jsi/jsi.h
if (NOT EXISTS "${HERMES_SRC_DIR}/API/jsi/jsi/jsi.h")
    message(FATAL_ERROR "HERMES_SRC_DIR does not contain API/jsi/jsi/jsi.h")
endif ()

if (NOT HERMES_BUILD_DIR)
    message(FATAL_ERROR "Please specify HERMES_BUILD_DIR")
endif ()
# On multi-config generators (e.g. Visual Studio), build artifacts are placed
# under a per-config subdirectory (e.g. Debug/).
if (CMAKE_CONFIGURATION_TYPES)
    set(HERMES_CONFIG_SUBDIR "/Debug")
else ()
    set(HERMES_CONFIG_SUBDIR "")
endif ()

# Validate HERMES_BUILD_DIR by checking for bin/hermes with the platform-specific extension
if (NOT EXISTS "${HERMES_BUILD_DIR}/bin${HERMES_CONFIG_SUBDIR}/hermes${CMAKE_EXECUTABLE_SUFFIX}")
    message(FATAL_ERROR "HERMES_BUILD_DIR does not contain bin${HERMES_CONFIG_SUBDIR}/hermes${CMAKE_EXECUTABLE_SUFFIX}")
endif ()

if (NOT NITRO_INCLUDE_DIR)
    message(FATAL_ERROR "Please specify NITRO_INCLUDE_DIR (NitroModules cpp/ source root)")
endif ()
if (NOT EXISTS "${NITRO_INCLUDE_DIR}/entrypoint/InstallNitro.hpp")
    message(FATAL_ERROR "NITRO_INCLUDE_DIR does not contain entrypoint/InstallNitro.hpp")
endif ()

if (NOT NITRO_LIB_DIR)
    message(FATAL_ERROR "Please specify NITRO_LIB_DIR (dir containing the NitroModules shared lib)")
endif ()

# Add Hermes include directories
include_directories("${HERMES_SRC_DIR}/API")
include_directories("${HERMES_SRC_DIR}/API/jsi")
include_directories("${HERMES_SRC_DIR}/public")
# uniffi-bindgen-react-native shared headers + stubs (for ReactCommon/CallInvoker.h).
include_directories("../includes")
include_directories("../stubs")
# Reuse MyCallInvoker.h and timers.js.inc from the JSI test-harness.
include_directories("../test-harness")
# Nitro headers — every subdirectory of cpp/ on the include path, mirroring
# what packages/react-native-nitro-modules/android/CMakeLists.txt does.
include_directories(
    "${NITRO_INCLUDE_DIR}/core"
    "${NITRO_INCLUDE_DIR}/entrypoint"
    "${NITRO_INCLUDE_DIR}/jsi"
    "${NITRO_INCLUDE_DIR}/platform"
    "${NITRO_INCLUDE_DIR}/prototype"
    "${NITRO_INCLUDE_DIR}/registry"
    "${NITRO_INCLUDE_DIR}/templates"
    "${NITRO_INCLUDE_DIR}/threading"
    "${NITRO_INCLUDE_DIR}/utils"
    "${NITRO_INCLUDE_DIR}/views"
)
# Flat `NitroModules/` mirror containing every Nitro .hpp — required by
# any generated source that uses `<NitroModules/Foo.hpp>` includes.
if (NITRO_FLAT_INCLUDE_DIR)
    include_directories("${NITRO_FLAT_INCLUDE_DIR}")
endif ()

# Add Hermes library directories
link_directories("${HERMES_BUILD_DIR}/API/hermes${HERMES_CONFIG_SUBDIR}")
link_directories("${HERMES_BUILD_DIR}/jsi${HERMES_CONFIG_SUBDIR}")
link_directories("${HERMES_BUILD_DIR}/lib${HERMES_CONFIG_SUBDIR}")
link_directories("${NITRO_LIB_DIR}")

link_libraries(jsi)

# Detect hermes library name and location, which changed in newer Hermes branches.
# (See the JSI test-harness CMakeLists for the full backstory.)
set(HERMES_LIB_SEARCH_PATHS
    "${HERMES_BUILD_DIR}/API/hermes${HERMES_CONFIG_SUBDIR}"
    "${HERMES_BUILD_DIR}/lib${HERMES_CONFIG_SUBDIR}"
)
find_library(HERMES_LIB NAMES hermes hermesvm PATHS ${HERMES_LIB_SEARCH_PATHS} NO_DEFAULT_PATH)
if (NOT HERMES_LIB)
    message(FATAL_ERROR "Could not find hermes or hermesvm library in ${HERMES_BUILD_DIR}")
endif ()
get_filename_component(HERMES_LIB_NAME "${HERMES_LIB}" NAME_WE)
# Strip the "lib" prefix if present (Unix), so we get the bare linker name.
string(REGEX REPLACE "^lib" "" HERMES_LIB_NAME "${HERMES_LIB_NAME}")

# Locate NitroModules shared lib. find_library handles the platform prefix/suffix:
# libNitroModules.so (Linux), libNitroModules.dylib (macOS), NitroModules.dll (Windows).
find_library(NITRO_LIB NAMES NitroModules PATHS "${NITRO_LIB_DIR}" NO_DEFAULT_PATH)
if (NOT NITRO_LIB)
    message(FATAL_ERROR "Could not find NitroModules library in ${NITRO_LIB_DIR}")
endif ()

add_executable(test-runner-nitro test-runner-nitro.cpp)
# Prevent windows.h from defining min/max macros that conflict with Hermes headers.
if (MSVC)
    target_compile_definitions(test-runner-nitro PRIVATE NOMINMAX)
endif ()
target_link_libraries(test-runner-nitro ${HERMES_LIB_NAME} ${NITRO_LIB})

# Make sure the loader can find libNitroModules at runtime on Unix without
# requiring the user to set LD_LIBRARY_PATH / DYLD_LIBRARY_PATH manually.
if (APPLE)
    set_target_properties(test-runner-nitro PROPERTIES
        BUILD_RPATH "${NITRO_LIB_DIR};${HERMES_BUILD_DIR}/API/hermes${HERMES_CONFIG_SUBDIR};${HERMES_BUILD_DIR}/lib${HERMES_CONFIG_SUBDIR}"
        INSTALL_RPATH "${NITRO_LIB_DIR};${HERMES_BUILD_DIR}/API/hermes${HERMES_CONFIG_SUBDIR};${HERMES_BUILD_DIR}/lib${HERMES_CONFIG_SUBDIR}"
    )
elseif (UNIX)
    set_target_properties(test-runner-nitro PROPERTIES
        BUILD_RPATH "${NITRO_LIB_DIR}:${HERMES_BUILD_DIR}/API/hermes${HERMES_CONFIG_SUBDIR}:${HERMES_BUILD_DIR}/lib${HERMES_CONFIG_SUBDIR}"
        INSTALL_RPATH "${NITRO_LIB_DIR}:${HERMES_BUILD_DIR}/API/hermes${HERMES_CONFIG_SUBDIR}:${HERMES_BUILD_DIR}/lib${HERMES_CONFIG_SUBDIR}"
    )
endif ()
