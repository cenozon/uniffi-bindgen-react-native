// SPDX-License-Identifier: MPL-2.0
//
// Process-global handle to the JS-thread `Dispatcher`.
//
// The event-driven RustFuture driver (`future.hpp`) needs to *defer* a
// continuation re-poll onto the JS thread's task queue (see the deadlock note
// there). Nitro's `Dispatcher` is the JS-thread task queue, but it is keyed by
// `jsi::Runtime&` (`Dispatcher::getRuntimeGlobalDispatcher`) and neither the
// C-ABI uniffi continuation nor a typed HybridObject async-method body is
// handed a runtime. A Nitro host runs a single JS runtime, so we cache that
// runtime's Dispatcher here once, at any point where a runtime IS in hand, and
// read it back from the runtime-less continuation.
//
// There are TWO capture sites, because the desktop-only one is not enough:
//
//   * `registerNatives(jsi::Runtime&)` (desktop test runner) — the host runner
//     dlsym-calls it after installing the Dispatcher. On a real RN/Expo app
//     `registerNatives` is NEVER called (the static-archive member carrying it
//     is reached via the `+load` autolinker for *registration*, but the
//     Dispatcher-capture line in `registerNatives` itself runs only on the
//     desktop path), so relying on it alone leaves `get_js_dispatcher()` null on
//     device and `future.hpp` takes the inline re-poll -> self-deadlock for any
//     re-suspending / awaited-foreign-async-callback future (audit bug #6).
//
//   * the foreign-async-callback converter (`js_async_callback.hpp` fromJSI),
//     which runs on the JS thread WITH a runtime in hand on device too. It
//     calls `ensure_js_dispatcher_from_runtime(runtime)` (below) just before the
//     awaited JS Promise is chained, so by the time the driving future can
//     re-suspend the Dispatcher is already cached. This is the device path.
//
// Stored as a `weak_ptr` so we never extend the Dispatcher's lifetime past the
// runtime's; callers `lock()` and treat a null result as "no JS thread to defer
// onto" (process teardown), falling back to an inline re-poll.

#pragma once

#include <NitroModules/Dispatcher.hpp>

#include <atomic>
#include <jsi/jsi.h>
#include <memory>
#include <mutex>

namespace ubrn::nitro {

namespace detail {
inline std::mutex &js_dispatcher_mutex() {
  static std::mutex m;
  return m;
}

inline std::weak_ptr<::margelo::nitro::Dispatcher> &js_dispatcher_slot() {
  static std::weak_ptr<::margelo::nitro::Dispatcher> slot;
  return slot;
}

/// Set once a live Dispatcher has been cached, so the hot async-callback path
/// can skip the mutex + JSI lookup after the first successful capture. Relaxed
/// is sufficient: it only gates a redundant re-capture of the same process-
/// global Dispatcher, never correctness.
inline std::atomic<bool> &js_dispatcher_captured() {
  static std::atomic<bool> captured{false};
  return captured;
}
} // namespace detail

/// Cache the JS-thread Dispatcher for later runtime-less access. Idempotent;
/// safe to call from any thread. Call once a runtime is available (the
/// generated `registerNatives` does this on desktop;
/// `ensure_js_dispatcher_from_runtime` does it on device).
inline void set_js_dispatcher(std::shared_ptr<::margelo::nitro::Dispatcher> dispatcher) {
  std::lock_guard<std::mutex> lock(detail::js_dispatcher_mutex());
  bool live = dispatcher != nullptr;
  detail::js_dispatcher_slot() = std::move(dispatcher);
  if (live) {
    detail::js_dispatcher_captured().store(true, std::memory_order_relaxed);
  }
}

/// The cached JS-thread Dispatcher, or `nullptr` if none has been set (or the
/// runtime has since torn down). Safe to call from any thread.
inline std::shared_ptr<::margelo::nitro::Dispatcher> get_js_dispatcher() {
  std::lock_guard<std::mutex> lock(detail::js_dispatcher_mutex());
  return detail::js_dispatcher_slot().lock();
}

/// Device-path capture: cache the runtime's global Dispatcher the first time we
/// are on the JS thread with a runtime in hand (the foreign-async-callback
/// converter). Cheap and idempotent — after the first successful capture this
/// is a single relaxed atomic load and returns immediately, so it is safe to
/// call on every async-callback conversion. A runtime without an installed
/// Dispatcher (older host) is a non-fatal no-op: the captured flag stays false
/// so a later call retries, and meanwhile the continuation falls back to an
/// inline re-poll. Must be called ONLY on the JS thread (where `runtime` is
/// valid) — `getRuntimeGlobalDispatcher` touches the runtime.
inline void ensure_js_dispatcher_from_runtime(facebook::jsi::Runtime &runtime) {
  if (detail::js_dispatcher_captured().load(std::memory_order_relaxed)) {
    return;
  }
  try {
    set_js_dispatcher(
        ::margelo::nitro::Dispatcher::getRuntimeGlobalDispatcher(runtime));
  } catch (...) {
    // No Dispatcher installed for this runtime yet — leave the captured flag
    // unset so a subsequent call retries; the continuation meanwhile falls back
    // to an inline re-poll (correct for single-poll futures).
  }
}

} // namespace ubrn::nitro
