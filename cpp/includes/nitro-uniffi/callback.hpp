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
  uint64_t insert(std::shared_ptr<HybridT> instance) {
    std::lock_guard<std::mutex> guard(mu_);
    uint64_t handle = next_++;
    if (handle == 0) {
      // Handle 0 is reserved as "null / not present" by uniffi's
      // wire format for optional callbacks. Skip it on wrap.
      handle = next_++;
    }
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
  // Start at 1 so 0 is reserved (uniffi treats handle 0 as "no
  // instance" for optional callback args).
  std::atomic<uint64_t> next_{1};
};

} // namespace ubrn::nitro
