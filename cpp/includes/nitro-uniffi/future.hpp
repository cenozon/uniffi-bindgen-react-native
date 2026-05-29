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
// complete + lift + `resolve()` directly (Nitro's `Promise::resolve` marshals
// to the JS thread itself, exactly as `Promise::async` relies on). A
// ready-on-first-poll future therefore resolves with zero thread hops.

#pragma once

#include <NitroModules/Promise.hpp>
#include <UniffiRustCallStatus.h>
#include <nitro-uniffi/js_dispatcher.hpp>

#include <cstdint>
#include <exception>
#include <functional>
#include <memory>
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
                        std::function<T()> &&complete) {
  auto promise = ::margelo::nitro::Promise<T>::create();
  auto *state = new RustFutureAsyncState{handle, poll_fn, nullptr};
  state->on_ready = [handle, free_fn, complete = std::move(complete),
                     promise]() {
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
                             std::function<void()> &&complete) {
  auto promise = ::margelo::nitro::Promise<void>::create();
  auto *state = new RustFutureAsyncState{handle, poll_fn, nullptr};
  state->on_ready = [handle, free_fn, complete = std::move(complete),
                     promise]() {
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
