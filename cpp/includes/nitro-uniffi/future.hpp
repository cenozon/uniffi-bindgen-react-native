// SPDX-License-Identifier: MPL-2.0
//
// Drive a uniffi `RustFuture` to completion from the Nitro backend, the
// *event-driven* way — no worker thread, no blocking wait.
//
// The uniffi C ABI for async functions has four pieces:
//
//   1. The `_fn_func_<name>` scaffolding returns a `uint64_t` handle the
//      first time it's called. Opaque — it identifies a RustFuture inside
//      uniffi-core's handle map.
//
//   2. `ffi_<crate>_rust_future_poll_<T>(handle, cb, data)` arms a
//      continuation: when the future next makes progress (or completes)
//      Rust calls `cb(data, poll_result)`. `poll_result == 0` (Ready) means
//      ready for completion; `poll_result == 1` (Wake) means poll again.
//
//   3. `ffi_<crate>_rust_future_complete_<T>(handle, &status)` extracts the
//      lowered return value (+ any typed-error buffer via `status`). Called
//      once, after the continuation fired Ready.
//
//   4. `ffi_<crate>_rust_future_free_<T>(handle)` drops the future's
//      handle-map entry, paired 1:1 with each `_fn_func_*` call.
//
// Earlier this ran the poll loop on a `Nitro` thread-pool worker and blocked
// it on a `std::condition_variable` (via `Promise::async`). That paid a
// thread dispatch + condvar + cross-thread resolve on *every* await — even
// for an immediately-ready future, which uniffi signals synchronously inside
// the first `poll`. Instead we now create a pending `Promise`, arm the
// continuation, and return immediately: when Rust fires the continuation we
// complete + lift + `resolve()` directly. A ready-on-first-poll future
// therefore resolves with zero thread hops.
//
// `Promise::resolve` does NOT itself marshal back to the JS thread — Nitro's
// `Promise` runs its resolved/rejected listeners *inline* on whatever thread
// calls `resolve()`/`reject()` (`react-native-nitro-modules` `core/Promise.hpp`).
// Thread-correctness here rests instead on the *poll-on-JS-thread discipline*:
// a `Wake` re-poll is deferred onto `get_js_dispatcher()->runAsync` (see
// `rust_future_async_continuation` below), so the terminal `Ready` continuation
// — and thus `on_ready()` / `resolve()` — runs on the JS thread. The one
// exception is the immediately-ready first poll, which fires synchronously
// inside the kick-off call that already runs on the JS thread. Either way
// `resolve()`/`reject()` is reached on the JS thread; never call them from a
// Rust executor thread.
//
// CANCELLATION (parity with the JSI backend's `uniffiRustCallAsync`):
// uniffi's async C ABI also exposes
// `ffi_<crate>_rust_future_cancel_<T>(handle)` — one arg, void return, no
// status. It transitions the RustFuture's scheduler to `Cancelled` and fires
// any armed continuation immediately with `Ready`; the subsequent `complete`
// then returns `RustCallStatus::cancelled()` (code 3), which `status.hpp`
// already maps to a rejecting `UniffiUnexpectedError`. `cancel` is idempotent
// and a no-op after the future settles, BUT its uniffi-documented safety
// contract requires the handle has NOT yet been passed to `free`. The JSI
// backend honours this by removing its abort listener BEFORE `freeFunc` in a
// `finally`; we honour the same ordering by *deregistering* this future from
// the process-global `CancelRegistry` inside `on_ready`, paired with and
// strictly BEFORE `free_fn(handle)` — so an abort that races completion can
// never reach a freed handle (a post-deregistration `abort_rust_future` is a
// registry miss = safe no-op).
//
// In JSI the whole poll loop lives in TS, so JS owns the bigint handle and can
// call `cancelFunc(rustFuture)` directly. Here the loop is in C++ and the
// handle never escapes to JS; Nitro's `Promise<T>` is strictly one-directional
// (no channel to push a cancel callback back into C++). So the conveyance is a
// process-global token registry plus a non-spec `__uniffiBeginAbortable()` /
// `__uniffiAbort(token)` HybridObject method pair (the same non-spec-method
// precedent as the callback `setJsImpl` hook): the wrapper calls
// `__uniffiBeginAbortable()` synchronously just before kicking off the typed
// async call (arming a thread-local token consumed by `drive_rust_future_async`),
// then registers `signal.addEventListener("abort", () => api.__uniffiAbort(token))`
// and removes it on settle.

#pragma once

#include <NitroModules/Promise.hpp>
// Quote-relative (not <angled>) so these resolve against this header's own
// location regardless of the consumer's HEADER_SEARCH_PATHS. The iOS pod
// is consumed via `:path` (node_modules), so every header is physically
// present next to this file, but only the top-level `cpp/includes` dir is
// reliably on the angle-include search path — an `<nitro-uniffi/...>` angle
// include of a sibling fails under CocoaPods' framework-module build (this
// is exactly clang's "use quotes instead" diagnostic). Matches the
// quote-relative convention `NitroUniffi.hpp` already uses for these splits.
#include "../UniffiRustCallStatus.h"
#include "js_dispatcher.hpp"

#include <cstdint>
#include <exception>
#include <functional>
#include <memory>
#include <mutex>
#include <unordered_map>
#include <utility>

namespace ubrn::nitro {

/// Poll-result codes — mirror `RustFuturePoll` on the Rust side
/// (`uniffi_core::ffi::rustfuture::RustFuturePoll`).
enum class RustFuturePoll : int8_t {
  Ready = 0,
  Wake = 1,
};

using PollFn = void (*)(uint64_t, void (*)(uint64_t, int8_t), uint64_t);
using FreeFutureFn = void (*)(uint64_t);
/// uniffi's `ffi_<crate>_rust_future_cancel_<T>` — one arg (the RustFuture
/// handle), void return, no status. Idempotent; safe to call after the future
/// settles, but NOT after `free` (uniffi's documented contract).
using CancelFutureFn = void (*)(uint64_t);

namespace detail {

/// Process-global registry mapping an opaque JS-facing cancel *token* to the
/// in-flight RustFuture handle + its `cancel` symbol. An entry exists only
/// between a future's kick-off and its terminal `on_ready` (which deregisters
/// strictly before `free_fn` runs), so `abort` can never reach a freed handle.
struct CancelEntry {
  uint64_t handle;
  CancelFutureFn cancel_fn;
};

inline std::mutex &cancel_registry_mutex() {
  static std::mutex m;
  return m;
}

inline std::unordered_map<uint64_t, CancelEntry> &cancel_registry() {
  static std::unordered_map<uint64_t, CancelEntry> registry;
  return registry;
}

/// Monotonic token source. `0` is reserved as "no token" (an un-armed call),
/// so the first real token is `1`.
inline uint64_t next_cancel_token() {
  std::lock_guard<std::mutex> lock(cancel_registry_mutex());
  static uint64_t counter = 0;
  return ++counter;
}

/// Thread-local "armed token" set by `begin_abortable_future()` on the JS
/// thread, consumed by the very next `drive_rust_future_async[/ _void]` kick-off
/// on the same thread. Both calls run synchronously on the JS thread with no
/// intervening await, so the slot is never observed by another future. `0`
/// means un-armed (the future is then registered with no cancel token and is
/// simply non-abortable, matching a JSI call made without an `AbortSignal`).
inline uint64_t &armed_cancel_token() {
  thread_local uint64_t token = 0;
  return token;
}

} // namespace detail

/// Arm the next async kick-off on this (JS) thread as abortable: allocate a
/// fresh token, stash it in the thread-local slot, and return it to JS. The
/// wrapper calls this synchronously immediately before the typed async call.
/// Reached from JS via a non-spec `__uniffiBeginAbortable()` HybridObject method
/// (the same non-spec-method precedent as the callback `setJsImpl` hook).
inline uint64_t begin_abortable_future() {
  uint64_t token = detail::next_cancel_token();
  detail::armed_cancel_token() = token;
  return token;
}

/// Abort the in-flight future registered under `token`, if any. A registry
/// miss (the future already settled and deregistered, or the token was never
/// registered) is a safe no-op — exactly the idempotent / post-settle behaviour
/// uniffi's `rust_future_cancel` guarantees. Reached from JS via a non-spec
/// `__uniffiAbort(token)` HybridObject method.
inline void abort_rust_future(uint64_t token) {
  CancelFutureFn cancel_fn = nullptr;
  uint64_t handle = 0;
  {
    std::lock_guard<std::mutex> lock(detail::cancel_registry_mutex());
    auto &registry = detail::cancel_registry();
    auto it = registry.find(token);
    if (it == registry.end()) {
      return;
    }
    cancel_fn = it->second.cancel_fn;
    handle = it->second.handle;
  }
  // Call cancel OUTSIDE the lock: it may synchronously fire the armed
  // continuation (`Ready`), whose `on_ready` deregisters this token — which
  // re-takes the registry mutex. Holding it here would self-deadlock.
  if (cancel_fn != nullptr) {
    cancel_fn(handle);
  }
}

namespace detail {

/// Register `{handle, cancel_fn}` under `token` so a later `abort_rust_future`
/// can reach it. No-op when `token == 0` (un-armed call) or `cancel_fn` is null.
inline void register_cancellable(uint64_t token, uint64_t handle,
                                 CancelFutureFn cancel_fn) {
  if (token == 0 || cancel_fn == nullptr) {
    return;
  }
  std::lock_guard<std::mutex> lock(cancel_registry_mutex());
  cancel_registry()[token] = CancelEntry{handle, cancel_fn};
}

/// Drop `token`'s registry entry. Called from `on_ready` strictly before
/// `free_fn(handle)`, preserving uniffi's "cancel-before-free" ordering. No-op
/// for `token == 0`.
inline void deregister_cancellable(uint64_t token) {
  if (token == 0) {
    return;
  }
  std::lock_guard<std::mutex> lock(cancel_registry_mutex());
  cancel_registry().erase(token);
}

/// Take (and clear) the thread-local armed token, returning `0` if un-armed.
/// Called once per kick-off, synchronously on the JS thread.
inline uint64_t take_armed_cancel_token() {
  uint64_t token = armed_cancel_token();
  armed_cancel_token() = 0;
  return token;
}

} // namespace detail

/// Type-erased state for one in-flight RustFuture. Heap-allocated and kept
/// alive across (re-)polls; `on_ready` carries the `T`-specific complete +
/// lift + resolve/reject closure, so the C-ABI continuation below needs no
/// template instantiation (a template can't be `extern "C"`).
struct RustFutureAsyncState {
  uint64_t handle;
  PollFn poll_fn;
  /// Completes + lifts the value and resolves/rejects the Promise. Does NOT
  /// free the future handle (the continuation does that around it) and does
  /// NOT delete `this`.
  std::function<void()> on_ready;
};

/// C-ABI continuation matching uniffi's
/// `RustFutureContinuationCallback = extern "C" fn(u64, RustFuturePoll)`.
/// On `Wake`, re-arm. On `Ready`, run the completion closure then drop the
/// state. May fire on the JS thread (synchronously inside `poll` for a
/// ready future) or a Rust executor thread; `Promise::resolve` handles the
/// thread marshaling.
///
/// The `Wake` re-poll is *deferred* onto the JS thread's task queue rather than
/// run inline. uniffi delivers a `Wake` from inside `Scheduler::wake()`, which
/// holds the RustFuture's *scheduler* mutex while it calls this continuation
/// (`uniffi_core` `rustfuture/scheduler.rs`). Re-polling inline re-enters
/// `RustFuture::poll`, which — if the future suspends again — calls
/// `Scheduler::store()` and re-locks that same (non-reentrant `std::sync`)
/// mutex on the same thread: a self-deadlock. This is exactly the path a
/// suspending future hits when an awaited foreign (JS) async callback completes
/// and wakes the driving future. Deferring the re-poll lets `wake()` unwind and
/// release the scheduler lock first; the deferred task then re-polls on a clean
/// stack. The immediately-ready case (`Ready` on the first poll) is unaffected
/// — it never re-polls; it runs `on_ready()` synchronously here. If no JS
/// Dispatcher is available (process teardown / a host without one), we fall
/// back to an inline re-poll so a wakeup is never silently dropped.
extern "C" inline void
rust_future_async_continuation(uint64_t cb_data, int8_t poll_result) noexcept {
  auto *state = reinterpret_cast<RustFutureAsyncState *>(cb_data);
  if (poll_result == static_cast<int8_t>(RustFuturePoll::Wake)) {
    if (auto dispatcher = get_js_dispatcher()) {
      dispatcher->runAsync([state]() {
        state->poll_fn(state->handle, &rust_future_async_continuation,
                       reinterpret_cast<uint64_t>(state));
      });
    } else {
      state->poll_fn(state->handle, &rust_future_async_continuation,
                     reinterpret_cast<uint64_t>(state));
    }
    return;
  }
  state->on_ready();
  delete state;
}

/// Value-returning futures. `complete` runs `complete_<T>` + status check +
/// lift, returning the C++ value (or throwing a typed/unexpected error).
template <typename T>
inline std::shared_ptr<::margelo::nitro::Promise<T>>
drive_rust_future_async(uint64_t handle, PollFn poll_fn, FreeFutureFn free_fn,
                        std::function<T()> &&complete,
                        CancelFutureFn cancel_fn = nullptr) {
  auto promise = ::margelo::nitro::Promise<T>::create();
  // Consume the thread-local token armed by the wrapper's
  // `__uniffiBeginAbortable()` (0 = un-armed / non-abortable), then register
  // this future so `__uniffiAbort(token)` can reach it.
  uint64_t cancel_token = detail::take_armed_cancel_token();
  detail::register_cancellable(cancel_token, handle, cancel_fn);
  auto *state = new RustFutureAsyncState{handle, poll_fn, nullptr};
  state->on_ready = [handle, free_fn, cancel_token, complete = std::move(complete),
                     promise]() {
    // Deregister BEFORE free_fn (uniffi cancel-before-free contract): after
    // this point an aborting `abort_rust_future` is a registry-miss no-op, so
    // it can never call `cancel` on the about-to-be-freed handle.
    detail::deregister_cancellable(cancel_token);
    try {
      T value = complete();
      free_fn(handle);
      promise->resolve(std::move(value));
    } catch (...) {
      free_fn(handle);
      promise->reject(std::current_exception());
    }
  };
  poll_fn(handle, &rust_future_async_continuation,
          reinterpret_cast<uint64_t>(state));
  return promise;
}

/// Void futures. `complete` runs `complete_void` + status check (no value).
inline std::shared_ptr<::margelo::nitro::Promise<void>>
drive_rust_future_async_void(uint64_t handle, PollFn poll_fn,
                             FreeFutureFn free_fn,
                             std::function<void()> &&complete,
                             CancelFutureFn cancel_fn = nullptr) {
  auto promise = ::margelo::nitro::Promise<void>::create();
  uint64_t cancel_token = detail::take_armed_cancel_token();
  detail::register_cancellable(cancel_token, handle, cancel_fn);
  auto *state = new RustFutureAsyncState{handle, poll_fn, nullptr};
  state->on_ready = [handle, free_fn, cancel_token, complete = std::move(complete),
                     promise]() {
    detail::deregister_cancellable(cancel_token);
    try {
      complete();
      free_fn(handle);
      promise->resolve();
    } catch (...) {
      free_fn(handle);
      promise->reject(std::current_exception());
    }
  };
  poll_fn(handle, &rust_future_async_continuation,
          reinterpret_cast<uint64_t>(state));
  return promise;
}

} // namespace ubrn::nitro
