// SPDX-License-Identifier: MPL-2.0
//
// Foreign-side callback object registry for the Nitro backend.
//
// uniffi's callback interface protocol expects foreign code to register
// a vtable of function pointers via a per-callback `init_fn`. Each
// vtable method takes a `uint64_t` handle as its first argument that
// identifies *which* foreign-side instance Rust is calling into. The
// foreign side has to maintain the handle <-> instance mapping itself.
//
// On the Rust-to-C++ direction (uniffi interfaces), the inverse problem
// is solved by `UniffiObjectHandle` in `handle.hpp` — we get a uint64
// back from Rust and hand it straight to a C++ wrapper for safe-keeping
// until destruction. Here we need to go the other way: a C++
// HybridObject (the user's foreign callback implementation, vended via
// Nitro's `shared_ptr<HybridT>`) needs a stable uint64 identity so Rust
// can name it across the FFI.
//
// `CallbackHandleMap<HybridT>` is the simplest workable shape:
//
//   * `insert(shared_ptr)` -> returns a fresh monotonically increasing
//     `uint64_t`. The map *owns* the shared_ptr — so as long as Rust
//     holds the handle, the C++ HybridObject stays alive.
//   * `get(handle)` -> looks up the shared_ptr for a method dispatch.
//     Returns the stored pointer; callers should not let it outlive
//     the dispatch.
//   * `remove(handle)` -> drops the entry. Called from the `free`
//     trampoline that the codegen emits as the last entry in the
//     vtable (uniffi guarantees `free` is the last call Rust makes for
//     a given handle).
//
// The map is global-singleton-per-type via `instance()` so the
// generated vtable trampolines can find it without a per-call lookup.
// It's thread-safe because uniffi vtable calls can come from any Rust
// thread.

#pragma once

#include <atomic>
#include <cstdint>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <unordered_map>

namespace ubrn::nitro {

/// Registry mapping `uint64_t` handles to foreign-side callback
/// instances (Nitro `HybridObject`s, owned as `std::shared_ptr<HybridT>`).
///
/// `HybridT` is the Nitrogen-generated spec base class — typically
/// `HybridFooCallbackSpec`. The generated code registers a vtable that
/// uses a CallbackHandleMap<HybridFooCallbackSpec> to dispatch.
template <typename HybridT> class CallbackHandleMap {
public:
  /// Singleton accessor. Lifetime is process lifetime — uniffi's
  /// callback registration semantics don't have a teardown moment
  /// that we could hook to drop the map.
  static CallbackHandleMap &instance() {
    static CallbackHandleMap inst;
    return inst;
  }

  /// Register a new instance and return its handle. The map takes
  /// shared ownership of the instance — callers can drop their own
  /// shared_ptr immediately and it'll stay alive until `remove` is
  /// called.
  ///
  /// Handles are **odd** values (1, 3, 5, …). This is load-bearing, not
  /// cosmetic: uniffi distinguishes foreign-generated handles from
  /// Rust-generated ones by the lowest bit. `Handle::is_foreign()` is
  /// `(raw & 1) == 1`, and a `with_foreign` trait's `try_lift` takes the
  /// foreign path (wrap the handle in a vtable-backed proxy) only when that
  /// bit is set — otherwise it treats the value as a leaked `Arc` pointer
  /// and does `Arc::from_raw(handle)`, which segfaults for a small integer.
  /// (uniffi-core `ffi/handle.rs`: "Foreign handles are generated with a
  /// handle map that only generates odd values." Mirrors the JSI runtime's
  /// `UniffiHandleMap`, which starts at 1 and steps by 2.)
  uint64_t insert(std::shared_ptr<HybridT> instance) {
    std::lock_guard<std::mutex> guard(mu_);
    uint64_t handle = next_;
    next_ += 2; // stay odd
    map_.emplace(handle, std::move(instance));
    return handle;
  }

  /// Look up the instance for a given handle. Throws if the handle
  /// is unknown — that would mean Rust held a stale handle past the
  /// `free` call, which is a uniffi protocol violation.
  std::shared_ptr<HybridT> get(uint64_t handle) const {
    std::lock_guard<std::mutex> guard(mu_);
    auto it = map_.find(handle);
    if (it == map_.end()) {
      throw std::runtime_error(
          "CallbackHandleMap: unknown handle (use-after-free?)");
    }
    return it->second;
  }

  /// Drop the entry for `handle`. After this, the underlying
  /// HybridObject's shared_ptr refcount drops by one — if the
  /// foreign side has already released its reference, the dtor
  /// fires here.
  void remove(uint64_t handle) {
    std::lock_guard<std::mutex> guard(mu_);
    map_.erase(handle);
  }

private:
  CallbackHandleMap() = default;

  mutable std::mutex mu_;
  std::unordered_map<uint64_t, std::shared_ptr<HybridT>> map_;
  // Start at 1 and step by 2 (in `insert`) so every handle is odd: 0 is
  // reserved (uniffi treats it as "no instance" for optional callback
  // args) and the lowest bit marks the handle as foreign — see `insert`.
  std::atomic<uint64_t> next_{1};
};

} // namespace ubrn::nitro
