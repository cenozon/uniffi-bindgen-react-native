// SPDX-License-Identifier: MPL-2.0
//
// Umbrella header. Generated `Hybrid<Interface>.cpp` files
// `#include <NitroUniffi.hpp>` and pull in every helper they might need —
// the granular splits in `nitro-uniffi/*.hpp` are for human readers, not
// the compiler.

#pragma once

// `rust_buffer.hpp` is the only one that includes `<RustBuffer.h>` —
// pull it in first so `UniffiRustCallStatus.h` (which references
// `RustBuffer`) can resolve the type. The remaining sub-headers all
// reference `UniffiRustCallStatus` in template parameter lists without
// including the header themselves; surface the include here too.
#include "nitro-uniffi/rust_buffer.hpp"
#include <UniffiRustCallStatus.h>

#include "nitro-uniffi/callback.hpp"
#include "nitro-uniffi/composites.hpp"
#include "nitro-uniffi/converters.hpp"
#include "nitro-uniffi/error_message.hpp"
#include "nitro-uniffi/future.hpp"
#include "nitro-uniffi/handle.hpp"
#include "nitro-uniffi/status.hpp"
