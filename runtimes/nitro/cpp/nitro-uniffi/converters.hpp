// SPDX-License-Identifier: MPL-2.0
//
// Lift/lower converters between Nitro's C++ surface and uniffi's C ABI.
//
// Primitives and `std::string` are the universal cases — every uniffi
// project uses them. They live here in the runtime so the per-project
// codegen doesn't have to re-emit them. Compound types (records, enums,
// sequences, optionals) are project-specific and so are emitted by the
// `gen_cpp_nitro` codegen — but they delegate field-by-field to the
// readers/writers in `rust_buffer.hpp`.
//
// Naming convention:
//
//   * `lower_*` — Nitro C++ value → uniffi C ABI value. Primitives pass
//     by value; compounds materialize a RustBuffer that the caller must
//     free.
//   * `lift_*` — uniffi C ABI value → Nitro C++ value. Compounds are read
//     from a `RustBufferView`, then the caller frees the buffer.

#pragma once

#include <UniffiRustCallStatus.h>

#include <chrono>
#include <cstdint>
#include <string>

#include "rust_buffer.hpp"

namespace ubrn::nitro {

// -----------------------------------------------------------------------
// Primitives.
//
// The uniffi C ABI passes primitives by value. Nitro's C++ method
// signatures use the same C++ types Nitrogen emits from the .nitro.ts
// spec — which match uniffi's primitive set 1:1. There's no per-call
// conversion work, but we expose the function shapes so the generated
// per-method body stays uniform across primitive vs compound returns.
// -----------------------------------------------------------------------

inline uint8_t  lower_u8 (uint8_t  v) noexcept { return v; }
inline uint16_t lower_u16(uint16_t v) noexcept { return v; }
inline uint32_t lower_u32(uint32_t v) noexcept { return v; }
inline uint64_t lower_u64(uint64_t v) noexcept { return v; }
inline int8_t   lower_i8 (int8_t   v) noexcept { return v; }
inline int16_t  lower_i16(int16_t  v) noexcept { return v; }
inline int32_t  lower_i32(int32_t  v) noexcept { return v; }
inline int64_t  lower_i64(int64_t  v) noexcept { return v; }
inline float    lower_f32(float    v) noexcept { return v; }
inline double   lower_f64(double   v) noexcept { return v; }
// uniffi wire-encodes bool as int8 to keep ABI portable.
inline int8_t   lower_bool(bool    v) noexcept { return v ? 1 : 0; }

inline uint8_t  lift_u8 (uint8_t  v) noexcept { return v; }
inline uint16_t lift_u16(uint16_t v) noexcept { return v; }
inline uint32_t lift_u32(uint32_t v) noexcept { return v; }
inline uint64_t lift_u64(uint64_t v) noexcept { return v; }
inline int8_t   lift_i8 (int8_t   v) noexcept { return v; }
inline int16_t  lift_i16(int16_t  v) noexcept { return v; }
inline int32_t  lift_i32(int32_t  v) noexcept { return v; }
inline int64_t  lift_i64(int64_t  v) noexcept { return v; }
inline float    lift_f32(float    v) noexcept { return v; }
inline double   lift_f64(double   v) noexcept { return v; }
inline bool     lift_bool(int8_t  v) noexcept { return v != 0; }

// -----------------------------------------------------------------------
// std::string.
//
// uniffi passes strings as `RustBuffer` (utf-8 bytes without a length
// prefix — `RustBuffer::len` is the length). Reading is direct from the
// buffer payload; writing requires an alloc via the namespace's
// `ffi_<crate>_rustbuffer_alloc` symbol, which the generated code threads
// in via template parameter.
// -----------------------------------------------------------------------

/// Lift a uniffi-returned string buffer into a `std::string`. The buffer
/// must be freed by the caller via the namespace-specific
/// `ffi_<crate>_rustbuffer_free` after this returns — typical pattern is
/// the generated method wraps the buffer in a `RustBufferOwned` (see
/// `rust_buffer.hpp`) which frees on scope exit.
inline std::string lift_string(RustBuffer buf) {
    if (buf.data == nullptr || buf.len == 0) {
        return {};
    }
    return std::string(reinterpret_cast<const char*>(buf.data),
                       static_cast<size_t>(buf.len));
}

// -----------------------------------------------------------------------
// std::chrono::system_clock::time_point  (uniffi Timestamp / SystemTime).
//
// Crosses the FFI as a `RustBuffer` whose payload is an `i64 seconds + u32
// nanos` (big-endian) pair. See `RustBufferReader::read_timestamp` /
// `RustBufferWriter::write_timestamp` for the magnitude/sign convention,
// which mirrors uniffi-rs's own `try_read`/`write`.
// -----------------------------------------------------------------------

inline std::chrono::system_clock::time_point lift_timestamp(RustBuffer buf) {
    RustBufferReader r{buf};
    return r.read_timestamp();
}

template <RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus*),
          RustBuffer (*Reserve)(RustBuffer, uint64_t, UniffiRustCallStatus*)>
inline RustBuffer lower_timestamp(std::chrono::system_clock::time_point tp) {
    RustBufferWriter<Alloc, Reserve> w;
    w.write_timestamp(tp);
    return w.finish();
}

// -----------------------------------------------------------------------
// Duration  (uniffi `std::time::Duration`).
//
// Crosses the FFI as a `RustBuffer` whose payload is `u64 seconds + u32
// nanos` (big-endian). The C++ surface is `double` milliseconds — matching
// the TS/JSI backend, which also exposes Duration as `number`.
// -----------------------------------------------------------------------

inline double lift_duration(RustBuffer buf) {
    RustBufferReader r{buf};
    return r.read_duration();
}

template <RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus*),
          RustBuffer (*Reserve)(RustBuffer, uint64_t, UniffiRustCallStatus*)>
inline RustBuffer lower_duration(double ms) {
    RustBufferWriter<Alloc, Reserve> w;
    w.write_duration(ms);
    return w.finish();
}

/// Lower a `std::string` to a uniffi `RustBuffer`. Allocates a new
/// Rust-owned buffer via the provided namespace allocator and copies
/// `s` into it. Ownership of the returned buffer transfers to the Rust
/// callee — *do not* free on the C++ side.
template <RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus*)>
inline RustBuffer lower_string(const std::string& s) {
    UniffiRustCallStatus status{};
    RustBuffer buf = Alloc(static_cast<uint64_t>(s.size()), &status);
    if (status.code != 0) {
        throw std::runtime_error("RustBuffer alloc for string lower failed");
    }
    if (!s.empty()) {
        std::memcpy(buf.data, s.data(), s.size());
        buf.len = static_cast<uint64_t>(s.size());
    } else {
        buf.len = 0;
    }
    return buf;
}

} // namespace ubrn::nitro
