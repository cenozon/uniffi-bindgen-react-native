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
// matching the JS shape `model.rs`'s `ts_type()` chooses — `Map<K, V>` for
// EVERY non-string key:
//
//   * integer keys (u8/i8/u16/i16/u32/i32/f32/f64) -> `Map<number, V>`,
//     i.e. a JS `Map` keyed by JS `number`. (Previously these marshalled
//     through a plain JS OBJECT / TS `Record<number, V>`, but a JS object
//     stringifies every property name — so a numeric-keyed object and the
//     `Map<K, V>` surface `ts_type()` now emits would disagree on runtime
//     shape. A real `Map` keeps the key a `number` and matches the TS type.)
//   * 64-bit keys (u64/i64) -> `Map<bigint, V>`, i.e. a JS `Map` keyed by
//     BigInt — JS `number` cannot hold a u64/i64 losslessly, so the key is a
//     `bigint`.
//
// Both build/consume a real JS `Map` (constructed via the global `Map`
// ctor, driven through `set` / `Array.from`), differing only in how the
// key crosses: a `number` `jsi::Value` for the small ints/floats, a
// `bigint` for the 64-bit ints.
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

#include <NitroModules/JSICache.hpp>
#include <NitroModules/JSIConverter.hpp>

#include <cstdint>
#include <jsi/jsi.h>
#include <type_traits>
#include <unordered_map>

namespace margelo::nitro {

using namespace facebook;

namespace ubrn_detail {

// The global `Map` constructor and `Array.from` don't change for a given
// runtime, so resolve each once and keep it alive in the per-runtime
// `JSICache` (a `BorrowingReference` into the cache's `OwningReference`)
// instead of walking `global()` on every map crossing. Returns a reference so
// the move-only `jsi::Function` is never copied.
inline const jsi::Function &cachedMapCtor(jsi::Runtime &rt) {
  static std::unordered_map<jsi::Runtime *, BorrowingReference<jsi::Function>>
      cache;
  auto it = cache.find(&rt);
  if (it != cache.end() && it->second != nullptr)
    return *it->second;
  auto shared = JSICache::getOrCreateCache(rt).makeShared(
      rt.global().getPropertyAsFunction(rt, "Map"));
  auto [pos, _] = cache.insert_or_assign(&rt, std::move(shared));
  return *pos->second;
}

inline const jsi::Function &cachedArrayFrom(jsi::Runtime &rt) {
  static std::unordered_map<jsi::Runtime *, BorrowingReference<jsi::Function>>
      cache;
  auto it = cache.find(&rt);
  if (it != cache.end() && it->second != nullptr)
    return *it->second;
  auto array = rt.global().getPropertyAsObject(rt, "Array");
  auto shared = JSICache::getOrCreateCache(rt).makeShared(
      array.getPropertyAsFunction(rt, "from"));
  auto [pos, _] = cache.insert_or_assign(&rt, std::move(shared));
  return *pos->second;
}

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

// Read a numeric key (the `0`th element of a JS `Map` entry pair) back into
// the C++ key type `K`. The key crosses as a JS `number`, so a plain
// `asNumber` + narrowing cast recovers it for every 8/16/32-bit int and
// float/double `K`.
template <typename K>
inline K number_key_from_value(jsi::Runtime &runtime, const jsi::Value &key) {
  return static_cast<K>(key.asNumber());
}

// Spell a numeric key as a JS `number` `jsi::Value` for `Map.set`.
template <typename K>
inline jsi::Value number_key_to_value(jsi::Runtime & /*runtime*/, K key) {
  return jsi::Value(static_cast<double>(key));
}

} // namespace ubrn_detail

// ---------------------------------------------------------------------
// Integer / float keyed maps  <>  JS `Map<number, V>`.
//
// JS objects stringify property names, so a numeric-keyed object would not
// match the `Map<number, V>` surface `ts_type()` emits. We construct a real
// `Map` via the global `Map` ctor and drive `set` / `Array.from` exactly as
// the 64-bit-key path below, differing only in the key crossing as a JS
// `number` rather than a `bigint`.
// ---------------------------------------------------------------------
template <typename KeyType, typename ValueType>
struct JSIConverter<
    std::unordered_map<KeyType, ValueType>,
    std::enable_if_t<ubrn_detail::is_number_map_key_v<KeyType>>>
    final {
  static inline std::unordered_map<KeyType, ValueType>
  fromJSI(jsi::Runtime &runtime, const jsi::Value &arg) {
    jsi::Object jsMap = arg.asObject(runtime);
    // `Array.from(map)` yields an array of `[key, value]` entry pairs —
    // avoids driving the JS iterator protocol by hand.
    const jsi::Function &arrayFrom = ubrn_detail::cachedArrayFrom(runtime);
    jsi::Array entries =
        arrayFrom.call(runtime, jsMap).asObject(runtime).asArray(runtime);
    size_t length = entries.size(runtime);

    std::unordered_map<KeyType, ValueType> map;
    map.reserve(length);
    for (size_t i = 0; i < length; ++i) {
      jsi::Array pair =
          entries.getValueAtIndex(runtime, i).asObject(runtime).asArray(
              runtime);
      jsi::Value key = pair.getValueAtIndex(runtime, 0);
      jsi::Value value = pair.getValueAtIndex(runtime, 1);
      map.emplace(ubrn_detail::number_key_from_value<KeyType>(runtime, key),
                  JSIConverter<ValueType>::fromJSI(runtime, value));
    }
    return map;
  }

  static inline jsi::Value
  toJSI(jsi::Runtime &runtime,
        const std::unordered_map<KeyType, ValueType> &map) {
    const jsi::Function &mapCtor = ubrn_detail::cachedMapCtor(runtime);
    jsi::Object jsMap = mapCtor.callAsConstructor(runtime).asObject(runtime);
    jsi::Function set = jsMap.getPropertyAsFunction(runtime, "set");
    for (const auto &pair : map) {
      jsi::Value key = ubrn_detail::number_key_to_value<KeyType>(runtime, pair.first);
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
    return object.instanceOf(runtime, ubrn_detail::cachedMapCtor(runtime));
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
    const jsi::Function &arrayFrom = ubrn_detail::cachedArrayFrom(runtime);
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
    const jsi::Function &mapCtor = ubrn_detail::cachedMapCtor(runtime);
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
    return object.instanceOf(runtime, ubrn_detail::cachedMapCtor(runtime));
  }
};

} // namespace margelo::nitro
