// SPDX-License-Identifier: MPL-2.0
//
// `error_field_to_string` — render a decoded uniffi error-variant field
// into a human-readable fragment for the C++ exception message.
//
// Non-flat uniffi error enums serialize their variants exactly like a
// tagged data-enum: an i32 tag (1-based) followed by the variant's fields
// in declaration order (uniffi_macros `rich_error_ffi_converter_impl`). The
// generated `lift_<Name>Error` decoder reads those fields back off the wire
// and embeds their values in the exception message — because Nitro's
// `HybridFunction::callMethod` translates a thrown `std::exception` into a
// `jsi::JSError` using only `what()`, so the message string is the *only*
// channel a payload can reach JS through (surfacing as `error.message`).
//
// These overloads cover every field type the generated error codec is
// allowed to decode (see `NitroType::is_error_message_decodable`): scalars,
// bool, string, bytes, date/duration, and composites of those. Records,
// data enums and interface handles are deliberately *not* surfaced and never
// reach here.

#pragma once

#include <NitroModules/ArrayBuffer.hpp>

#include <chrono>
#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <type_traits>
#include <unordered_map>
#include <vector>

namespace ubrn::nitro {

// ---- Scalars / bool ----
inline std::string error_field_to_string(bool v) { return v ? "true" : "false"; }
inline std::string error_field_to_string(uint8_t v) { return std::to_string(static_cast<unsigned>(v)); }
inline std::string error_field_to_string(uint16_t v) { return std::to_string(v); }
inline std::string error_field_to_string(uint32_t v) { return std::to_string(v); }
inline std::string error_field_to_string(uint64_t v) { return std::to_string(v); }
inline std::string error_field_to_string(int8_t v) { return std::to_string(static_cast<int>(v)); }
inline std::string error_field_to_string(int16_t v) { return std::to_string(v); }
inline std::string error_field_to_string(int32_t v) { return std::to_string(v); }
inline std::string error_field_to_string(int64_t v) { return std::to_string(v); }
inline std::string error_field_to_string(float v) { return std::to_string(v); }
inline std::string error_field_to_string(double v) { return std::to_string(v); }

// ---- String ----
// Quote so the boundary of the value is unambiguous in the joined message.
inline std::string error_field_to_string(const std::string& v) {
  return "\"" + v + "\"";
}

// ---- Bytes (Vec<u8> -> ArrayBuffer) ----
// Don't splatter raw (possibly binary) bytes into the message; report the
// size, which is the useful diagnostic.
inline std::string error_field_to_string(const std::shared_ptr<::margelo::nitro::ArrayBuffer>& v) {
  return std::string("ArrayBuffer(") + (v ? std::to_string(v->size()) : std::string("null")) +
         " bytes)";
}

// ---- Interface handles (uniffi `interface` -> `std::shared_ptr<HybridXxx>`)
// A record / data-enum payload field can itself be an interface handle, e.g.
// coverall's `Repair{ patch: Arc<Patch> }` or `SimpleDict{ coveralls:
// Option<Arc<Coveralls>> }`. When such a record is reachable inside an error
// variant's payload, the generated record `error_field_to_string` recurses
// into every field — including the interface handle. The handle's underlying
// Rust value cannot be cheaply rendered (it lives behind the FFI as an opaque
// pointer), so we surface a non-null/null placeholder rather than the value.
//
// SFINAE-excluded for `ArrayBuffer` so the dedicated bytes overload above
// stays the unique best match for `shared_ptr<ArrayBuffer>` (this template
// would otherwise be an equally-good candidate and make the call ambiguous).
template <typename T,
          typename = std::enable_if_t<
              !std::is_same_v<T, ::margelo::nitro::ArrayBuffer>>>
std::string error_field_to_string(const std::shared_ptr<T>& v) {
  return v ? std::string("<HybridObject>") : std::string("null");
}

// ---- Timestamp (SystemTime) ----
inline std::string
error_field_to_string(const std::chrono::system_clock::time_point& v) {
  auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(v.time_since_epoch()).count();
  return std::string("Date(") + std::to_string(static_cast<long long>(ms)) + "ms)";
}

// ---- Composites: optional / vector / map ----
//
// Forward-declare ALL THREE composite templates before any of their bodies.
// The composite bodies call `error_field_to_string` UNQUALIFIED on their
// element type, so for a composite WRAPPING a sibling composite (e.g.
// `optional<vector<T>>`, `optional<unordered_map<K,V>>`, `vector<map<K,V>>`)
// the inner call is resolved at instantiation by two-phase lookup: candidates
// are (a) names visible at the *definition point* of the OUTER composite plus
// (b) ADL on the argument. The argument is a `std::vector` / `std::optional` /
// `std::unordered_map`, whose only associated namespace is `std` (no
// `error_field_to_string` there), so ADL cannot reach the sibling — and absent
// these forward declarations the sibling is also not yet visible at the outer
// template's definition point if it is declared later in this file. That is the
// exact clang error "call to function 'error_field_to_string' that is neither
// visible in the template definition nor found by argument-dependent lookup".
// Declaring every composite up-front makes the whole composite-of-composite
// chain order-independent (a partial reorder is insufficient: whichever
// composite is declared last would still be unreachable from the others).
//
// The scalar / string / ArrayBuffer / shared_ptr / timestamp overloads above
// already precede the composites, so they remain visible at every definition
// point and need no forward declaration. The per-record / per-enum generated
// overloads (in the namespace codecs header) are co-located with their type in
// `margelo::nitro::<ns>` and are found by ADL on the argument — they are a
// separate, working resolution path and are unaffected by this block.
template <typename T>
std::string error_field_to_string(const std::optional<T>& v);
template <typename T>
std::string error_field_to_string(const std::vector<T>& v);
template <typename K, typename V>
std::string error_field_to_string(const std::unordered_map<K, V>& v);

template <typename T>
std::string error_field_to_string(const std::optional<T>& v) {
  return v.has_value() ? error_field_to_string(*v) : std::string("null");
}

template <typename T>
std::string error_field_to_string(const std::vector<T>& v) {
  std::string out = "[";
  for (size_t i = 0; i < v.size(); ++i) {
    if (i != 0) {
      out += ", ";
    }
    out += error_field_to_string(v[i]);
  }
  out += "]";
  return out;
}

template <typename K, typename V>
std::string error_field_to_string(const std::unordered_map<K, V>& v) {
  std::string out = "{";
  bool first = true;
  for (const auto& [k, val] : v) {
    if (!first) {
      out += ", ";
    }
    first = false;
    out += error_field_to_string(k);
    out += ": ";
    out += error_field_to_string(val);
  }
  out += "}";
  return out;
}

} // namespace ubrn::nitro
