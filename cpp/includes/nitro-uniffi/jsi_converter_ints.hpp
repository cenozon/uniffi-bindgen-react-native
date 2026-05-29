// SPDX-License-Identifier: MPL-2.0
//
// JSIConverter specializations for the fixed-width integer types Nitro
// core's `JSIConverter.hpp` does not ship.
//
// Nitro core provides converters for `int` (== `int32_t`), `int64_t`,
// `uint64_t`, `double`, `float`, `bool` and `std::string`. uniffi's type
// universe also includes `u8/u16/u32` and `i8/i16` — which map to JS
// `number`. Every one of those fits exactly in an IEEE-754 double (max
// `u32` is 4_294_967_295 < 2^53), so the conversion is lossless and we
// can route them through `jsi::Value`'s number channel exactly like
// Nitro's own `int` / `float` converters do.
//
// `int32_t` is `int` on every platform we target, so it is intentionally
// NOT re-specialized here (that would be a redefinition of Nitro core's
// `JSIConverter<int>`). `u64` / `i64` stay on Nitro core's BigInt path.
//
// Generated record / enum headers include this so their per-field
// `JSIConverter<T>` lookups resolve for the full uniffi integer set.

#pragma once

#include <NitroModules/JSIConverter.hpp>

#include <cstdint>
#include <type_traits>

namespace margelo::nitro {

using namespace facebook;

namespace ubrn_detail {
// Shared body for the small-integer converters: marshal through the JS
// number channel with a static_cast on each side. `canConvert` mirrors
// Nitro core — any JS number is accepted (range is not policed, matching
// the rest of the converters).
template <typename T> struct NumberLikeConverter {
  static inline T fromJSI(jsi::Runtime &, const jsi::Value &arg) {
    return static_cast<T>(arg.asNumber());
  }
  static inline jsi::Value toJSI(jsi::Runtime &, T arg) {
    return jsi::Value(static_cast<double>(arg));
  }
  static inline bool canConvert(jsi::Runtime &, const jsi::Value &value) {
    return value.isNumber();
  }
};
} // namespace ubrn_detail

template <>
struct JSIConverter<uint8_t> final : ubrn_detail::NumberLikeConverter<uint8_t> {
};
template <>
struct JSIConverter<int8_t> final : ubrn_detail::NumberLikeConverter<int8_t> {};
template <>
struct JSIConverter<uint16_t> final
    : ubrn_detail::NumberLikeConverter<uint16_t> {};
template <>
struct JSIConverter<int16_t> final : ubrn_detail::NumberLikeConverter<int16_t> {
};
template <>
struct JSIConverter<uint32_t> final
    : ubrn_detail::NumberLikeConverter<uint32_t> {};

} // namespace margelo::nitro
