// SPDX-License-Identifier: MPL-2.0
//
// Interned tag-value cache for uniffi tagged-/flat-enum converters.
//
// A uniffi enum crosses to JS either as a string union (flat enums) or as a
// discriminated union `{ tag: "<VariantName>", ... }` (tagged enums). In both
// cases the variant tag is a fixed ASCII literal known at compile time, so its
// `jsi::String` can be allocated once per (runtime, tag) and reused instead of
// building a fresh `jsi::String` on every `toJSI` crossing.
//
// `ubrnInternedTag` returns a `const jsi::Value&` referencing a String that is
// kept alive by the per-runtime `JSICache` (a `BorrowingReference` into the
// cache's `OwningReference`). Both the inline enum converter (`<Enum>.hpp`) and
// the out-of-line cycle-footer converter (`<Enum>.conv.hpp`) include this
// header so the common (non-cycle) path interns exactly like the cycle path.

#pragma once

#include <NitroModules/JSICache.hpp>

#include <unordered_map>

namespace margelo::nitro {

#ifndef UBRN_INTERNED_TAG
#define UBRN_INTERNED_TAG 1
inline const jsi::Value& ubrnInternedTag(jsi::Runtime& runtime, const char* tag) {
  static std::unordered_map<jsi::Runtime*,
      std::unordered_map<const char*, BorrowingReference<jsi::Value>>> cache;
  auto& perRt = cache[&runtime];
  auto it = perRt.find(tag);
  if (it != perRt.end() && it->second != nullptr) return *it->second;
  auto shared = JSICache::getOrCreateCache(runtime)
                    .makeShared(jsi::Value(jsi::String::createFromAscii(runtime, tag)));
  auto [pos, _] = perRt.insert_or_assign(tag, std::move(shared));
  return *pos->second;
}
#endif

} // namespace margelo::nitro
