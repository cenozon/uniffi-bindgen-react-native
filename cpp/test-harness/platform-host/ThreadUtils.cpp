// SPDX-License-Identifier: MPL-2.0
//
// Host implementation of `margelo::nitro::ThreadUtils`. Nitro's core
// expects per-platform impls of this class (Android's lives in
// `react-native-nitro-modules/android/.../platform/ThreadUtils.cpp`,
// iOS's in `.../ios/platform/ThreadUtils.cpp`). For our bare-Hermes test
// runner — which runs on the host OS to drive emitted bindings from cargo
// tests — we provide a minimal stub.
//
// The semantics are simplified: the **process-startup thread** is treated
// as the "UI thread", and the UI-thread `Dispatcher` runs tasks
// synchronously on whatever thread calls it. That's enough for the
// fixture-test surface — `Promise<T>::async` paths still spawn on the
// ThreadPool worker, sync setup paths still run inline, and JS-thread
// callbacks land on the same thread the test runner drives the event
// loop on.

#include "ThreadUtils.hpp"

#include <atomic>
#include <pthread.h>
#include <string>
#include <thread>

#include "Dispatcher.hpp"

namespace margelo::nitro {

namespace {

// Captured at process load; whichever thread runs static initializers
// first becomes the "UI thread" for the purposes of `isUIThread`.
const std::thread::id g_ui_thread_id = std::this_thread::get_id();

/// Minimal `Dispatcher` that runs everything synchronously on the caller's
/// thread. Sufficient for host test runs where the JS thread + the
/// "UI thread" are the same thread, and where we don't need to round-trip
/// scheduling work through a real run loop.
class InlineDispatcher final : public Dispatcher {
public:
  void runSync(std::function<void()> &&fn) override { fn(); }
  void runAsync(std::function<void()> &&fn) override { fn(); }
};

} // namespace

std::string ThreadUtils::getThreadName() {
#if defined(__linux__) || defined(__APPLE__)
  char buffer[64] = {0};
  if (pthread_getname_np(pthread_self(), buffer, sizeof(buffer)) == 0) {
    return std::string(buffer);
  }
#endif
  return std::string("host-thread");
}

void ThreadUtils::setThreadName(const std::string &name) {
#if defined(__linux__)
  // Linux's pthread_setname_np caps at 15 bytes + NUL.
  pthread_setname_np(pthread_self(), name.substr(0, 15).c_str());
#elif defined(__APPLE__)
  // macOS only lets a thread name itself.
  pthread_setname_np(name.c_str());
#else
  (void)name;
#endif
}

bool ThreadUtils::isUIThread() {
  return std::this_thread::get_id() == g_ui_thread_id;
}

std::shared_ptr<Dispatcher> ThreadUtils::createUIThreadDispatcher() {
  return std::make_shared<InlineDispatcher>();
}

} // namespace margelo::nitro
