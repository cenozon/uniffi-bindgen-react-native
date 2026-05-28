// SPDX-License-Identifier: MPL-2.0
//
// `RustCallStatus` → C++ exception bridge. Every uniffi C-ABI call writes a
// status code into an out-param of this shape:
//
//   struct RustCallStatus {
//     int8_t  code;       // 0=ok 1=error 2=unexpected_error 3=cancelled
//     RustBuffer error_buf; // populated when code == 1 with the serialized
//                           // error variant; *we* must free it
//   };
//
// In the JSI-host-object path this is unpacked by the per-FFI-call lambda in
// `wrapper.cpp` and surfaced via a JS Error. Under the Nitro-native path the
// HybridObject method body checks status inline and throws a C++ exception,
// which Nitro's `HybridFunction::callMethod` then translates into a
// `jsi::JSError` for JS.
//
// This header is intentionally lean — error *typing* (which uniffi error
// variant the buffer encodes) is the job of the per-error generated code,
// emitted by ubrn's gen_cpp_nitro backend. All this header does is the
// status-code dispatch + safe error-buffer release.

#pragma once

#include <RustBuffer.h>
#include <UniffiRustCallStatus.h>

#include <stdexcept>
#include <string>

namespace ubrn::nitro {

/// Status codes uniffi can write into `RustCallStatus::code`. Mirrors the
/// `RustCallStatusCode` enum on the Rust side
/// (uniffi_core::ffi::rustcalls::RustCallStatusCode).
enum class RustCallStatusCode : int8_t {
    Success = 0,
    Error = 1,
    UnexpectedError = 2,
    Cancelled = 3,
};

/// Exception thrown when uniffi returns a typed error (`code == 1`). Holds
/// onto the `RustBuffer` so the generated per-method wrapper can decode it
/// into the project-specific error variant before propagating. After
/// decoding the wrapper *must* call `release()` to hand the buffer back to
/// Rust; the destructor otherwise leaks the buffer because we can't safely
/// invoke a Rust allocator deallocation hook from a destructor.
class UniffiTypedError : public std::runtime_error {
public:
    UniffiTypedError(RustBuffer error_buf)
        : std::runtime_error("uniffi typed error (decode the error buffer)"),
          error_buf_(error_buf),
          released_(false) {}

    /// Hand ownership of the buffer to the caller. The caller is responsible
    /// for freeing it via the project's per-namespace
    /// `ffi_<crate>_rustbuffer_free` symbol after decoding.
    RustBuffer release() {
        released_ = true;
        return error_buf_;
    }

    ~UniffiTypedError() override = default;

private:
    RustBuffer error_buf_;
    bool released_;
};

/// Exception thrown for `code == 2`/`code == 3` paths — the Rust side
/// panicked or the foreign callback was cancelled. The message buffer is
/// utf8-decoded inline and freed before this exception is constructed, so
/// it carries no further state.
class UniffiUnexpectedError : public std::runtime_error {
public:
    using std::runtime_error::runtime_error;
};

/// Default-construct a status struct in the "no error yet" state. The C-ABI
/// requires status be zero-initialized — `{}` does that, but spelling it
/// out makes the precondition obvious in generated code.
inline UniffiRustCallStatus make_status() noexcept {
    UniffiRustCallStatus s{};
    s.code = static_cast<int8_t>(RustCallStatusCode::Success);
    s.error_buf = RustBuffer{};
    return s;
}

// Forward declaration — definition is below `check_status`; without this
// forward decl the unqualified `decode_string_and_free` call inside the
// `check_status` template body fails name lookup at the point of use.
template <typename FreeFn>
inline std::string decode_string_and_free(RustBuffer buf, FreeFn&& free_buffer);

/// Post-call status check. If the call succeeded, returns immediately. If
/// it raised a typed error, throws `UniffiTypedError` carrying the error
/// buffer for the caller's decoder. If it raised an unexpected error or was
/// cancelled, decodes the status buffer as a utf8 string (via the
/// project's `ffi_<crate>_rustbuffer_free` callback you pass in), frees it,
/// and throws `UniffiUnexpectedError`.
///
/// `free_buffer` is parameterized because the free symbol is per-namespace
/// (`ffi_<crate>_rustbuffer_free`) — the generated code captures the right
/// one in a lambda at the call site.
template <typename FreeFn>
inline void check_status(const UniffiRustCallStatus& status, FreeFn&& free_buffer) {
    switch (static_cast<RustCallStatusCode>(status.code)) {
        case RustCallStatusCode::Success:
            return;
        case RustCallStatusCode::Error:
            // Hand the buffer off to the generated per-error decoder. We
            // can't decode it ourselves — the variant shape is
            // project-specific.
            throw UniffiTypedError(status.error_buf);
        case RustCallStatusCode::UnexpectedError: {
            std::string message = decode_string_and_free(status.error_buf, free_buffer);
            throw UniffiUnexpectedError(std::move(message));
        }
        case RustCallStatusCode::Cancelled:
            throw UniffiUnexpectedError("uniffi call cancelled");
    }
    // The C ABI is in principle open-ended on `code` — defend the default.
    throw UniffiUnexpectedError("uniffi call returned an unknown status code");
}

/// utf8-decode a uniffi RustBuffer into a `std::string`, then free the
/// buffer via the supplied namespace-specific free callback. The buffer
/// layout matches `String::from_utf8` on the Rust side: bytes [0, len)
/// from `data`, with `capacity` being the alloc-size (may exceed `len`).
template <typename FreeFn>
inline std::string decode_string_and_free(RustBuffer buf, FreeFn&& free_buffer) {
    std::string out;
    if (buf.data != nullptr && buf.len > 0) {
        out.assign(reinterpret_cast<const char*>(buf.data),
                   static_cast<size_t>(buf.len));
    }
    free_buffer(buf);
    return out;
}

} // namespace ubrn::nitro
