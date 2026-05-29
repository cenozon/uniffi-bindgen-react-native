// SPDX-License-Identifier: MPL-2.0
//
// Opaque handle (uint64) wrapping for uniffi-interface objects under the
// Nitro-native backend.
//
// uniffi represents each user-defined `interface` as an opaque Arc-counted
// pointer on the Rust side. Across the C ABI it's passed as a `uint64_t`
// handle; the foreign language is expected to:
//
//   1. Treat the handle as an opaque token.
//   2. Call `ffi_<crate>_<obj>_clone(h)` before passing it back into a
//      method that *retains* a copy (no-op on Rust side beyond
//      `Arc::clone`).
//   3. Call `uniffi_<crate>_fn_free_<obj>(h)` exactly once when the
//      foreign-side wrapper is destructed.
//
// In the JSI-host-object path this is managed by `UniffiObjectFactory<T>`
// in the typescript runtime + a `FinalizationRegistry`. Under
// Nitro-native, each uniffi interface becomes a Nitro HybridObject — and
// HybridObjects are already lifetime-managed as `std::shared_ptr` on the
// C++ side. So the natural place to hang the `ffi_*_free_*` call is the
// HybridObject's destructor: when JS lets go of the last reference, the
// shared_ptr count hits zero, the dtor runs, the handle is freed.
//
// `UniffiObjectHandle` is a RAII wrapper around the `uint64_t` that
// captures the per-interface free-symbol via template parameter. The
// generated `Hybrid<Foo>` class holds a `UniffiObjectHandle<&free_foo>` as
// its sole instance state.

#pragma once

#include <UniffiRustCallStatus.h>

#include <cstdint>
#include <utility>

namespace ubrn::nitro {

/// Owning handle to a uniffi-side Arc<Object>. `FreeFn` is the
/// project+interface-specific `uniffi_<crate>_fn_free_<obj>` symbol.
///
/// Move-only — copying would double-free on dtor. The generated code
/// emits a `clone()` helper using the project's
/// `uniffi_<crate>_fn_clone_<obj>` when explicit reference duplication is
/// needed (e.g., passing the same object across two method boundaries in
/// one call).
template <void (*FreeFn)(uint64_t, UniffiRustCallStatus *)>
class UniffiObjectHandle {
public:
  UniffiObjectHandle() noexcept : raw_(0) {}
  explicit UniffiObjectHandle(uint64_t raw) noexcept : raw_(raw) {}

  UniffiObjectHandle(const UniffiObjectHandle &) = delete;
  UniffiObjectHandle &operator=(const UniffiObjectHandle &) = delete;

  UniffiObjectHandle(UniffiObjectHandle &&other) noexcept : raw_(other.raw_) {
    other.raw_ = 0;
  }

  UniffiObjectHandle &operator=(UniffiObjectHandle &&other) noexcept {
    if (this != &other) {
      release();
      raw_ = std::exchange(other.raw_, 0);
    }
    return *this;
  }

  ~UniffiObjectHandle() noexcept { release(); }

  uint64_t raw() const noexcept { return raw_; }
  explicit operator bool() const noexcept { return raw_ != 0; }

  /// Drop the handle without invoking `FreeFn` — used when the handle
  /// is being handed off to Rust (e.g., as a method argument that the
  /// callee will take ownership of).
  uint64_t take() noexcept { return std::exchange(raw_, 0); }

private:
  void release() noexcept {
    if (raw_ != 0) {
      UniffiRustCallStatus status{};
      FreeFn(raw_, &status);
      // Free hooks are infallible on the Rust side — they can only
      // panic, and a panic in the dtor is unrecoverable. We swallow
      // status here for the same reason `std::shared_ptr`'s
      // deleter is noexcept.
      (void)status;
      raw_ = 0;
    }
  }

  uint64_t raw_;
};

} // namespace ubrn::nitro
