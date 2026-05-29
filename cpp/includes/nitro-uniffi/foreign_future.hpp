// SPDX-License-Identifier: MPL-2.0
//
// C++ mirror of uniffi 0.31's *foreign-future* ABI — the protocol Rust uses
// to drive an `async fn` on a `#[uniffi::export(with_foreign)]` trait that is
// implemented in foreign (JS) code.
//
// When Rust calls such an async trait method through our callback vtable, it
// does NOT use the synchronous out-return / status-out shape. Instead (per
// `uniffi_macros::export::callback_interface`) the vtable entry is:
//
//   extern "C" void method(
//       uint64_t uniffi_handle,
//       <each arg lowered to its FFI type>,
//       ForeignFutureCallback<RetFfiType> uniffi_callback,
//       uint64_t uniffi_callback_data,
//       ForeignFutureDroppedCallbackStruct* uniffi_out_dropped_callback);
//
// The method returns `void` immediately. When the foreign async work
// completes, we invoke `uniffi_callback(uniffi_callback_data, result)` with a
// by-value `ForeignFutureResult<RetFfiType>` carrying the lowered return value
// (or a typed/unexpected error in the embedded `RustCallStatus`). The
// `uniffi_out_dropped_callback` out-param lets us register a cancellation hook;
// we default it to a no-op.
//
// The layouts below MUST match `uniffi_core-0.31.1/src/ffi/foreignfuture.rs`
// exactly (all `#[repr(C)]`):
//
//   pub type ForeignFutureCallback<FfiType> =
//       extern "C" fn(oneshot_handle: u64, ForeignFutureResult<FfiType>);
//   #[repr(C)] pub struct ForeignFutureResult<T> { return_value: T, call_status: RustCallStatus }
//   pub type ForeignFutureDroppedCallback = extern "C" fn(data: u64);
//   #[repr(C)] pub struct ForeignFutureDroppedCallbackStruct { callback_data: u64, callback: ForeignFutureDroppedCallback }
//
// NB: `ForeignFutureDroppedCallbackStruct` has a Rust `Drop` impl that *calls*
// `self.callback(self.callback_data)` — so the `callback` pointer must be a
// real (no-op) function, never null, or Rust will jump through a null pointer
// when it drops the struct.

#pragma once

#include <RustBuffer.h>
#include <UniffiRustCallStatus.h>

#include <cstdint>

namespace ubrn::nitro {

// C struct mirroring uniffi's `ForeignFutureResult<T>`: the lowered return
// value followed by the call status. Field order + `#[repr(C)]` layout are
// load-bearing — Rust reads this struct by value off the ABI.
template <typename T>
struct ForeignFutureResult {
  T return_value;
  UniffiRustCallStatus call_status;
};

// Specialization for `()` / void returns. uniffi's comment: "for void
// returns, T is `()`, which isn't directly representable with C since it's a
// ZST. Foreign code should treat that case as if there was no `return_value`
// field." So this carries only `call_status`.
template <>
struct ForeignFutureResult<void> {
  UniffiRustCallStatus call_status;
};

// Continuation function pointer. Called once when the foreign future settles;
// the result is passed BY VALUE (matching uniffi's `extern "C" fn(u64,
// ForeignFutureResult<T>)`). The C and C++ calling conventions are identical
// on every target we ship, so a plain function-pointer typedef is ABI-correct
// here (same precedent as `PollFn` in `future.hpp`).
template <typename T>
using ForeignFutureCallback = void (*)(uint64_t oneshot_handle,
                                       ForeignFutureResult<T> result);

// Called when the Rust side of the future is dropped (cancellation hook).
using ForeignFutureDroppedCallback = void (*)(uint64_t data);

// Mirrors `ForeignFutureDroppedCallbackStruct`. Rust's `Drop` impl invokes
// `callback(callback_data)`, so `callback` must point at a live function.
struct ForeignFutureDroppedCallbackStruct {
  uint64_t callback_data;
  ForeignFutureDroppedCallback callback;
};

// No-op dropped-callback target. We don't implement cancellation yet, so the
// dropped-callback we hand Rust does nothing — but it must be a real function
// (never null) because Rust calls it unconditionally on drop.
inline void noop_foreign_future_dropped_callback(uint64_t /* data */) noexcept {}

// The default no-op dropped-callback struct (matches uniffi's
// `ForeignFutureDroppedCallbackStruct::default()` shape: zero data + a no-op
// function pointer).
inline ForeignFutureDroppedCallbackStruct default_foreign_future_dropped_callback() noexcept {
  return ForeignFutureDroppedCallbackStruct{0, &noop_foreign_future_dropped_callback};
}

} // namespace ubrn::nitro
