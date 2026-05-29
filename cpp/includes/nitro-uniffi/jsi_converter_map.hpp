// SPDX-License-Identifier: MPL-2.0
//
// JSIConverter specializations for `std::unordered_map<K, V>` whose key
// `K` is NOT `std::string`.
//
// Nitro core (`JSIConverter+UnorderedMap.hpp`) only ships a converter for
// `std::unordered_map<std::string, V>` (marshalled as a JS plain object —
// a TS `Record<string, V>`). uniffi, however, supports `HashMap<K, V>`
// for any hashable `K`, e.g. `HashMap<u32, u64>` (coverall's
// `get_dict3`). `model.rs`'s `cxx_type()` emits `std::unordered_map<K, V>`
// for any key, but without a matching converter the JSI boundary
// (method arg/return, record/enum field) would fall through to Nitro
// core's primary template `static_assert` — a hard compile error.
//
// The RustBuffer wire codec (`composites.hpp` write_map/read_map) already
// round-trips any key; the only gap is the JS boundary. We fill it here,
// matching the JS shapes `model.rs`'s `ts_type()` chooses:
//
//   * integer keys (u8/i8/u16/i16/u32/i32/f32/f64) -> `Record<number, V>`,
//     i.e. a JS OBJECT whose property names are the stringified numeric
//     keys. We marshal exactly like Nitro's string-keyed converter but
//     parse each property name back to the numeric `K`.
//   * 64-bit keys (u64/i64) -> `Map<bigint, V>`, i.e. a JS `Map` keyed by
//     BigInt — JS objects cannot have bigint property names, so the only
//     faithful shape is a real `Map`.
//
// Both are partial specializations on the SFINAE `Enable` slot of Nitro
// core's primary `JSIConverter<T, Enable>` template, gated so they only
// ever match arithmetic (non-bool) keys. `std::string` is not arithmetic,
// so neither matches it and Nitro core's string specialization remains the
// unique best match — no ambiguity. For an arithmetic key Nitro's string
// specialization is not viable, so exactly one of ours applies.
//
// `V` is delegated to `JSIConverter<V>` and so composes with any value
// type (record / optional / vector / nested map / ...). Generated record /
// enum / interface headers include this alongside `jsi_converter_ints.hpp`
// (and after Nitro core's `JSIConverter.hpp`) so the small-integer value
// converters are already visible.

#pragma once

#include <NitroModules/JSIConverter.hpp>
#include <NitroModules/JSIHelpers.hpp>
#include <NitroModules/PropNameIDCache.hpp>

#include <cstdint>
#include <jsi/jsi.h>
#include <string>
#include <type_traits>
#include <unordered_map>

namespace margelo::nitro {

using namespace facebook;

namespace ubrn_detail {

// A map key is handled here iff `K` is arithmetic and not `bool`. `bool`
// is excluded because uniffi never produces a `bool`-keyed map and a JS
// `boolean` is not a sensible Record/Map key. `std::string` is not
// arithmetic, so it is excluded automatically and stays on Nitro core's
// own `std::unordered_map<std::string, V>` specialization.
template <typename K>
inline constexpr bool is_nonstring_map_key_v =
    std::is_arithmetic_v<K> && !std::is_same_v<K, bool>;

// 64-bit integer keys marshal through a JS `Map<bigint, V>`; every other
// supported key (8/16/32-bit ints, float, double) fits losslessly in a JS
// `number` and marshals through a plain object (`Record<number, V>`).
template <typename K>
inline constexpr bool is_bigint_map_key_v =
    std::is_integral_v<K> && (sizeof(K) == 8) && !std::is_same_v<K, bool>;

template <typename K>
inline constexpr bool is_number_map_key_v =
    is_nonstring_map_key_v<K> && !is_bigint_map_key_v<K>;

// Parse a JS object property name (always a string) back to the numeric
// key type `K`. Property names round-trip through JS's own number ->
// string coercion, so `std::sto*` / `std::strtod` recover them exactly.
template <typename K> inline K parse_number_key(const std::string &name) {
  if constexpr (std::is_floating_point_v<K>) {
    return static_cast<K>(std::strtod(name.c_str(), nullptr));
  } else if constexpr (std::is_signed_v<K>) {
    return static_cast<K>(std::strtoll(name.c_str(), nullptr, 10));
  } else {
    return static_cast<K>(std::strtoull(name.c_str(), nullptr, 10));
  }
}

// Spell a numeric key as a JS object property name. `std::to_string`
// matches JS's integer/float -> string coercion for the value ranges
// uniffi keys span.
template <typename K> inline std::string number_key_to_string(K key) {
  return std::to_string(key);
}

} // namespace ubrn_detail

// ---------------------------------------------------------------------
// Integer / float keyed maps  <>  JS object  (TS `Record<number, V>`).
// ---------------------------------------------------------------------
template <typename KeyType, typename ValueType>
struct JSIConverter<
    std::unordered_map<KeyType, ValueType>,
    std::enable_if_t<ubrn_detail::is_number_map_key_v<KeyType>>>
    final {
  static inline std::unordered_map<KeyType, ValueType>
  fromJSI(jsi::Runtime &runtime, const jsi::Value &arg) {
    jsi::Object object = arg.asObject(runtime);
    jsi::Array propertyNames = object.getPropertyNames(runtime);
    size_t length = propertyNames.size(runtime);

    std::unordered_map<KeyType, ValueType> map;
    map.reserve(length);
    for (size_t i = 0; i < length; ++i) {
      std::string name =
          propertyNames.getValueAtIndex(runtime, i).asString(runtime).utf8(
              runtime);
      jsi::Value value =
          object.getProperty(runtime, PropNameIDCache::get(runtime, name));
      map.emplace(ubrn_detail::parse_number_key<KeyType>(name),
                  JSIConverter<ValueType>::fromJSI(runtime, value));
    }
    return map;
  }

  static inline jsi::Value
  toJSI(jsi::Runtime &runtime,
        const std::unordered_map<KeyType, ValueType> &map) {
    jsi::Object object(runtime);
    for (const auto &pair : map) {
      std::string name = ubrn_detail::number_key_to_string<KeyType>(pair.first);
      jsi::Value value = JSIConverter<ValueType>::toJSI(runtime, pair.second);
      object.setProperty(runtime, PropNameIDCache::get(runtime, name),
                         std::move(value));
    }
    return object;
  }

  static inline bool canConvert(jsi::Runtime &runtime,
                                const jsi::Value &value) {
    if (!value.isObject()) {
      return false;
    }
    jsi::Object object = value.getObject(runtime);
    if (!isPlainObject(runtime, object)) {
      return false;
    }
    jsi::Array propNames = object.getPropertyNames(runtime);
    size_t size = propNames.size(runtime);
    for (size_t i = 0; i < size; i++) {
      std::string name =
          propNames.getValueAtIndex(runtime, i).asString(runtime).utf8(runtime);
      jsi::Value propValue =
          object.getProperty(runtime, PropNameIDCache::get(runtime, name));
      if (!JSIConverter<ValueType>::canConvert(runtime, propValue)) {
        return false;
      }
    }
    return true;
  }
};

// ---------------------------------------------------------------------
// 64-bit-integer keyed maps  <>  JS `Map<bigint, V>`.
//
// JS object property names cannot be bigint, so a u64/i64 key must use a
// real `Map`. We construct one via the global `Map` constructor and drive
// `set` / `forEach` through the function-call JSI surface.
// ---------------------------------------------------------------------
template <typename KeyType, typename ValueType>
struct JSIConverter<
    std::unordered_map<KeyType, ValueType>,
    std::enable_if_t<ubrn_detail::is_bigint_map_key_v<KeyType>>>
    final {
  static inline KeyType keyFromValue(jsi::Runtime &runtime,
                                     const jsi::Value &key) {
    if constexpr (std::is_signed_v<KeyType>) {
      return static_cast<KeyType>(key.asBigInt(runtime).asInt64(runtime));
    } else {
      return static_cast<KeyType>(key.asBigInt(runtime).asUint64(runtime));
    }
  }

  static inline jsi::Value keyToValue(jsi::Runtime &runtime, KeyType key) {
    if constexpr (std::is_signed_v<KeyType>) {
      return jsi::BigInt::fromInt64(runtime, static_cast<int64_t>(key));
    } else {
      return jsi::BigInt::fromUint64(runtime, static_cast<uint64_t>(key));
    }
  }

  static inline std::unordered_map<KeyType, ValueType>
  fromJSI(jsi::Runtime &runtime, const jsi::Value &arg) {
    jsi::Object jsMap = arg.asObject(runtime);
    // `[...map]` yields an array of `[key, value]` entry pairs. Spell it
    // through the iterator-free `Array.from(map)` to avoid driving the JS
    // iterator protocol by hand.
    jsi::Function arrayFrom = runtime.global()
                                  .getPropertyAsObject(runtime, "Array")
                                  .getPropertyAsFunction(runtime, "from");
    jsi::Array entries = arrayFrom.call(runtime, jsMap)
                             .asObject(runtime)
                             .asArray(runtime);
    size_t length = entries.size(runtime);

    std::unordered_map<KeyType, ValueType> map;
    map.reserve(length);
    for (size_t i = 0; i < length; ++i) {
      jsi::Array pair =
          entries.getValueAtIndex(runtime, i).asObject(runtime).asArray(
              runtime);
      jsi::Value key = pair.getValueAtIndex(runtime, 0);
      jsi::Value value = pair.getValueAtIndex(runtime, 1);
      map.emplace(keyFromValue(runtime, key),
                  JSIConverter<ValueType>::fromJSI(runtime, value));
    }
    return map;
  }

  static inline jsi::Value
  toJSI(jsi::Runtime &runtime,
        const std::unordered_map<KeyType, ValueType> &map) {
    jsi::Function mapCtor =
        runtime.global().getPropertyAsFunction(runtime, "Map");
    jsi::Object jsMap = mapCtor.callAsConstructor(runtime).asObject(runtime);
    jsi::Function set = jsMap.getPropertyAsFunction(runtime, "set");
    for (const auto &pair : map) {
      jsi::Value key = keyToValue(runtime, pair.first);
      jsi::Value value = JSIConverter<ValueType>::toJSI(runtime, pair.second);
      set.callWithThis(runtime, jsMap, std::move(key), std::move(value));
    }
    return jsMap;
  }

  static inline bool canConvert(jsi::Runtime &runtime,
                                const jsi::Value &value) {
    if (!value.isObject()) {
      return false;
    }
    jsi::Object object = value.getObject(runtime);
    // A JS `Map` instance — `instanceof global.Map`.
    jsi::Object mapCtor = runtime.global().getPropertyAsObject(runtime, "Map");
    return object.instanceOf(runtime, mapCtor.asFunction(runtime));
  }
};

} // namespace margelo::nitro
