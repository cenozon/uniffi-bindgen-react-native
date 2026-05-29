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
// runtime's Dispatcher here once, at a point where a runtime IS in hand
// (`registerNatives(jsi::Runtime&)` — invoked after the host installs the
// Dispatcher), and read it back from the runtime-less continuation.
//
// Stored as a `weak_ptr` so we never extend the Dispatcher's lifetime past the
// runtime's; callers `lock()` and treat a null result as "no JS thread to defer
// onto" (process teardown), falling back to an inline re-poll.

#pragma once

#include <NitroModules/Dispatcher.hpp>

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
} // namespace detail

/// Cache the JS-thread Dispatcher for later runtime-less access. Idempotent;
/// safe to call from any thread. Call once a runtime is available (the
/// generated `registerNatives` does this).
inline void set_js_dispatcher(std::shared_ptr<::margelo::nitro::Dispatcher> dispatcher) {
  std::lock_guard<std::mutex> lock(detail::js_dispatcher_mutex());
  detail::js_dispatcher_slot() = std::move(dispatcher);
}

/// The cached JS-thread Dispatcher, or `nullptr` if none has been set (or the
/// runtime has since torn down). Safe to call from any thread.
inline std::shared_ptr<::margelo::nitro::Dispatcher> get_js_dispatcher() {
  std::lock_guard<std::mutex> lock(detail::js_dispatcher_mutex());
  return detail::js_dispatcher_slot().lock();
}

} // namespace ubrn::nitro
