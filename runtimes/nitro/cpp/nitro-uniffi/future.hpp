// SPDX-License-Identifier: MPL-2.0
//
// Drive a uniffi `RustFuture` to completion from a foreign-language
// caller. The uniffi C ABI for async functions has three pieces:
//
//   1. The `_fn_func_<name>` scaffolding returns a `uint64_t` handle the
//      first time it's called. The handle is opaque — it identifies a
//      RustFuture inside uniffi-core's handle map.
//
//   2. `ffi_<crate>_rust_future_poll_<T>(handle, cb, data)` re-arms a
//      continuation: when the future next makes progress (or completes)
//      Rust calls `cb(data, poll_result)`. `poll_result == 0` means the
//      future is ready for completion; `poll_result == 1` means the
//      future woke spuriously and the foreign side should poll again.
//
//   3. `ffi_<crate>_rust_future_complete_<T>(handle, &status)` extracts
//      the lowered return value (and any typed-error buffer via
//      `status`). Must only be called once the continuation has fired
//      with `poll_result == 0`.
//
//   4. `ffi_<crate>_rust_future_free_<T>(handle)` drops the future's
//      handle-map entry, paired exactly once with each `_fn_func_*`
//      call. Must be called even on error paths.
//
// Foreign bindings drive this loop in their own concurrency primitive
// (Swift's `withUnsafeContinuation`, Python's `asyncio.Future`, ...).
// Under the Nitro backend we run the loop on a `Nitro` thread-pool
// worker (via `Promise<T>::async`), so the natural primitive is a
// `std::condition_variable` — block the worker thread, wake it from
// the C-ABI continuation callback. That's what `drive_rust_future`
// does; the generated method body wraps the call in a `Promise::async`
// closure that calls `drive_rust_future`, then `_complete_*`, then
// `_free_*`, then lifts the result.
//
// Why a separate helper at all? Two reasons:
//
//   * The continuation callback's `cb_data: uint64_t` argument is too
//     narrow to carry a `std::condition_variable*` directly on 64-bit
//     targets that map pointers to a smaller address space (e.g. tagged
//     pointer schemes — not common on Android/iOS, but a sound rule).
//     We funnel the pointer through a `reinterpret_cast<uint64_t>`,
//     which is well-defined per the C standard, and the round-trip
//     happens entirely inside this header.
//
//   * Centralizing the loop keeps the generated method bodies short
//     and means future changes to the poll-cycle (cancellation,
//     timeouts) land in one place rather than every template.

#pragma once

#include <UniffiRustCallStatus.h>

#include <condition_variable>
#include <cstdint>
#include <mutex>

namespace ubrn::nitro {

/// Poll-result codes — mirror `RustFuturePoll` on the Rust side
/// (`uniffi_core::ffi::rustfuture::RustFuturePoll`).
enum class RustFuturePoll : int8_t {
    /// The future has completed. The foreign side should call
    /// `rust_future_complete_*` to extract the result.
    Ready = 0,
    /// The future woke up but isn't done. The foreign side should call
    /// `rust_future_poll_*` again to re-arm the continuation.
    Wake = 1,
};

/// State block held alive for the duration of a single poll/complete
/// cycle. `cb_data` is a pointer to this struct, reinterpret-cast to
/// `uint64_t` for the C ABI.
struct RustFutureContinuation {
    std::mutex mutex;
    std::condition_variable cv;
    /// Set to true once the C-ABI continuation callback has fired.
    bool fired = false;
    /// The poll code Rust last reported.
    int8_t poll_result = 0;
};

/// C-ABI compatible continuation. Signature matches uniffi's
/// `RustFutureContinuationCallback = extern "C" fn(u64, RustFuturePoll)`.
///
/// `cb_data` is the reinterpret-cast pointer to a `RustFutureContinuation`
/// owned on the calling thread. Rust invokes this from *some* thread —
/// possibly the same worker we're blocking on, possibly a different
/// one — so we have to take the mutex before signalling.
extern "C" inline void
rust_future_continuation_trampoline(uint64_t cb_data, int8_t poll_result) noexcept {
    auto* state = reinterpret_cast<RustFutureContinuation*>(cb_data);
    {
        std::lock_guard<std::mutex> lock(state->mutex);
        state->poll_result = poll_result;
        state->fired = true;
    }
    state->cv.notify_one();
}

/// Pointer to the per-namespace `ffi_<crate>_rust_future_poll_<T>`
/// symbol. Captured per-call by the generated method body; we accept it
/// here as a template argument so the compiler can inline it.
using PollFn = void (*)(uint64_t, void (*)(uint64_t, int8_t), uint64_t);

/// Block the current thread until the uniffi RustFuture identified by
/// `handle` reports `RustFuturePoll::Ready`. Spurious `Wake` results
/// re-arm the continuation; this is the same loop Swift's
/// `uniffiRustCallAsync` runs, only built around a condvar instead of
/// `withUnsafeContinuation`.
///
/// The caller is responsible for calling `rust_future_complete_*` and
/// then `rust_future_free_*` after this returns. We deliberately don't
/// do those calls here — the complete-FFI's return type is
/// `T`-dependent, and the free-symbol is paired 1:1 with the eager
/// `_fn_func_*` call the method body already made.
inline void drive_rust_future(uint64_t handle, PollFn poll_fn) {
    RustFutureContinuation state;
    while (true) {
        // Reset for this iteration. We must reset *before* re-arming
        // the continuation, otherwise Rust could fire the callback
        // before we sit on the condvar and we'd miss the wake.
        {
            std::lock_guard<std::mutex> lock(state.mutex);
            state.fired = false;
            state.poll_result = 0;
        }

        poll_fn(handle,
                &rust_future_continuation_trampoline,
                reinterpret_cast<uint64_t>(&state));

        // Wait for the continuation to fire. The `wait` predicate is
        // checked under the mutex, so the wake from the trampoline is
        // race-free.
        {
            std::unique_lock<std::mutex> lock(state.mutex);
            state.cv.wait(lock, [&state] { return state.fired; });
        }

        if (state.poll_result == static_cast<int8_t>(RustFuturePoll::Ready)) {
            return;
        }
        // Else `Wake` — loop and re-poll.
    }
}

} // namespace ubrn::nitro
