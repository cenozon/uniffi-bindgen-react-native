// SPDX-License-Identifier: MPL-2.0
//
// Awaiting a JS-implemented async foreign-callback method's `Promise<T>`.
//
// Background. A `#[uniffi::export(with_foreign)]` trait's `async fn` is
// implemented in JS as an `async` method returning a JS `Promise`. The
// generated `Hybrid<Name>` async virtual returns
// `std::shared_ptr<ForeignAsyncResult<T>>` (this header's wrapper), and the
// `setJsImpl` hook binds the JS method as a `std::function` returning that
// same wrapper.
//
// Why a wrapper, and not `std::shared_ptr<Promise<T>>` directly.
// Nitro's `JSIConverter<std::function<R(Args...)>>` (see
// `react-native-nitro-modules/cpp/jsi/JSIConverter+Function.hpp`) branches on
// `is_promise_v<R>`: when the bound function's return type is a
// `Promise<T>` / `std::shared_ptr<Promise<T>>`, it builds an
// `AsyncJSCallback<T>` whose `SyncJSCallback<T>::call` reads the JS function's
// return value with `JSIConverter<T>::fromJSI` — i.e. it reads the *JS Promise
// object* as the raw `T` (e.g. a `uint32_t`) WITHOUT awaiting it. The Promise
// is silently dropped and the lifted value is garbage / never settles.
//
// `ForeignAsyncResult<T>` is deliberately NOT a `Promise`, so `is_promise_v`
// is false and Nitro keeps the bound function as a plain `SyncJSCallback`.
// `SyncJSCallback<std::shared_ptr<ForeignAsyncResult<T>>>::call` then converts
// the JS function's return value with the `JSIConverter` specialization below,
// which delegates to `JSIConverter<std::shared_ptr<Promise<T>>>::fromJSI`
// (`JSIConverter+Promise.hpp`) — the path that correctly chains `.then` /
// `.catch` listeners onto the JS Promise and yields a C++ `Promise<T>` that
// settles when the JS Promise does.
//
// The conversion runs on the JS thread (the `SyncJSCallback` is invoked from
// the async vtable trampoline, which Rust calls inside the first `poll` on the
// JS thread). The resulting `Promise<T>` is what the async trampoline chains
// `addOnResolvedListener` / `addOnRejectedListener` on to fire uniffi's
// foreign-future callback once the JS Promise settles.

#pragma once

#include <NitroModules/JSIConverter.hpp>
#include <NitroModules/Promise.hpp>

#include <jsi/jsi.h>
#include <memory>
#include <utility>

namespace ubrn::nitro {

// Thin wrapper around the C++ `Promise<T>` produced by chaining the JS
// Promise's `.then` / `.catch`. Holding a `Promise<T>` directly here (rather
// than wrapping it) would re-trigger Nitro's promise-unwrapping branch, so the
// wrapper is intentionally a distinct, non-`Promise` type.
template <typename T>
struct ForeignAsyncResult {
  std::shared_ptr<::margelo::nitro::Promise<T>> promise;
};

} // namespace ubrn::nitro

namespace margelo::nitro {

using namespace facebook;

// `JSIConverter` for the async-callback wrapper. `fromJSI` is the load-bearing
// direction: it receives the JS function's return value (a JS `Promise`) and
// delegates to the stock `JSIConverter<std::shared_ptr<Promise<T>>>::fromJSI`,
// which chains `.then` / `.catch` on the JS Promise and returns a C++
// `Promise<T>` that settles with the lifted value (or rejection). `toJSI` is
// provided for completeness (it would surface the inner Promise) but is never
// exercised on the foreign-callback path.
template <typename T>
struct JSIConverter<std::shared_ptr<::ubrn::nitro::ForeignAsyncResult<T>>> final {
  static inline std::shared_ptr<::ubrn::nitro::ForeignAsyncResult<T>>
  fromJSI(jsi::Runtime& runtime, const jsi::Value& value) {
    auto result = std::make_shared<::ubrn::nitro::ForeignAsyncResult<T>>();
    result->promise =
        JSIConverter<std::shared_ptr<Promise<T>>>::fromJSI(runtime, value);
    return result;
  }

  static inline jsi::Value
  toJSI(jsi::Runtime& runtime,
        const std::shared_ptr<::ubrn::nitro::ForeignAsyncResult<T>>& result) {
    return JSIConverter<std::shared_ptr<Promise<T>>>::toJSI(runtime,
                                                            result->promise);
  }

  static inline bool canConvert(jsi::Runtime& runtime, const jsi::Value& value) {
    return JSIConverter<std::shared_ptr<Promise<T>>>::canConvert(runtime, value);
  }
};

} // namespace margelo::nitro
