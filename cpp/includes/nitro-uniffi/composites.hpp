// SPDX-License-Identifier: MPL-2.0
//
// Lift/lower converters for uniffi composite types under the Nitro
// backend. Composites (Option / Vec / HashMap / Vec<u8>) all cross the
// uniffi C ABI as `RustBuffer` payloads with the same big-endian wire
// format that records / enums use; what's variant per-composite is the
// shape of the encoded payload.
//
// Uniffi's wire format:
//
//   * `Option<T>` -> tag byte (0 = None, 1 = Some) then, if Some, `T`.
//   * `Vec<T>`    -> i32 length, then `len` copies of `T` concatenated.
//   * `HashMap<K,V>` -> i32 length, then `len` (K, V) pairs concatenated.
//   * `Vec<u8>`   -> i32 length, then `len` raw bytes (no per-element
//     encoding).
//
// The per-element lift/lower functions are passed in as *non-type*
// template parameters (function pointers), so this header doesn't need
// any direct knowledge of the element type — the generated code
// stitches the recursion together at the call site:
//
//     lower_optional<&lower_u32>(opt)
//     lift_sequence<uint32_t, &lift_u32>(buf)
//     lower_map<&write_string, &write_u32>(map)
//
// Each element fn is one of:
//
//   * a primitive `lower_u8 / lift_u8 / ...` from `converters.hpp`
//   * the generated `write_<RecordName>(writer, value)` /
//     `read_<RecordName>(reader) -> value` helpers from
//     `<namespace>_codecs.hpp`
//   * the writer/reader thunks defined in this file for recursive
//     composites (`write_optional<F>`, etc.) — they have a uniform
//     `(RustBufferWriter&, const T&)` / `(RustBufferReader&) -> T`
//     signature so they nest cleanly.
//
// The top-level `lower_<composite><Alloc, Reserve, ...>` /
// `lift_<composite><...>` functions own the `RustBuffer` lifecycle. The
// inner `write_<composite>` / `read_<composite>` thunks operate on an
// already-open reader / writer and are what gets passed as the per-
// element function pointer when composites nest.

#pragma once

#include <UniffiRustCallStatus.h>

#include <cstdint>
#include <optional>
#include <string>
#include <unordered_map>
#include <vector>

#include "converters.hpp"
#include "rust_buffer.hpp"

namespace ubrn::nitro {

// -----------------------------------------------------------------------
// Primitive writer / reader thunks.
//
// `converters.hpp` exposes `lower_u32(uint32_t) -> uint32_t` etc. — those
// are no-ops, used as the function-pointer template argument when a
// composite holds a primitive at the *top* of a recursion. But when a
// primitive lives *inside* a composite (e.g. `Option<u32>`), uniffi
// encodes it in the big-endian wire format, not the C-ABI scalar form.
// So we expose a second flavor of helpers that operate on the
// `RustBufferReader` / `RustBufferWriter` cursor:
//
//   write_<prim>(writer, value)  -> void
//   read_<prim>(reader)          -> value
//
// The generated code picks the right family based on where the type
// sits in the recursion. Primitives at the FFI boundary stay as
// `lower_u32` (cheap), primitives inside a composite become
// `write_u32` (wire-encoded).
// -----------------------------------------------------------------------

template <RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*Reserve)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
using Writer = RustBufferWriter<Alloc, Reserve>;

#define UBRN_NITRO_PRIM_THUNK(T, suffix)                                       \
  template <RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),                 \
            RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>     \
  inline void write_##suffix(Writer<A, R> &w, T v) {                           \
    w.write_##suffix(v);                                                       \
  }                                                                            \
  inline T read_##suffix(RustBufferReader &r) { return r.read_##suffix(); }

UBRN_NITRO_PRIM_THUNK(uint8_t, u8)
UBRN_NITRO_PRIM_THUNK(uint16_t, u16)
UBRN_NITRO_PRIM_THUNK(uint32_t, u32)
UBRN_NITRO_PRIM_THUNK(uint64_t, u64)
UBRN_NITRO_PRIM_THUNK(int8_t, i8)
UBRN_NITRO_PRIM_THUNK(int16_t, i16)
UBRN_NITRO_PRIM_THUNK(int32_t, i32)
UBRN_NITRO_PRIM_THUNK(int64_t, i64)
UBRN_NITRO_PRIM_THUNK(float, f32)
UBRN_NITRO_PRIM_THUNK(double, f64)
UBRN_NITRO_PRIM_THUNK(bool, bool)

#undef UBRN_NITRO_PRIM_THUNK

// std::string thunks — wire-encoded as i32 length + utf-8 bytes.
template <RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline void write_string(Writer<A, R> &w, const std::string &s) {
  w.write_string(s);
}

inline std::string read_string(RustBufferReader &r) { return r.read_string(); }

// Timestamp / SystemTime thunks — wire-encoded as i64 seconds + u32 nanos.
template <RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline void write_timestamp(Writer<A, R> &w,
                            const std::chrono::system_clock::time_point &tp) {
  w.write_timestamp(tp);
}

inline std::chrono::system_clock::time_point
read_timestamp(RustBufferReader &r) {
  return r.read_timestamp();
}

// Duration thunks — wire-encoded as u64 seconds + u32 nanos. The C++
// surface is a `double` millisecond count.
template <RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline void write_duration(Writer<A, R> &w, const double &ms) {
  w.write_duration(ms);
}

inline double read_duration(RustBufferReader &r) { return r.read_duration(); }

// -----------------------------------------------------------------------
// Optional<T>.
//
// Wire format: tag byte (0/1), then payload if 1. The element fn is a
// `(reader)->T` or `(writer, const T&)` thunk so this composes
// recursively for `Option<Option<T>>`, `Option<Vec<T>>`, etc.
// -----------------------------------------------------------------------

template <typename T, RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *),
          void (*WriteInner)(Writer<A, R> &, const T &)>
inline void write_optional(Writer<A, R> &w, const std::optional<T> &v) {
  if (v.has_value()) {
    w.write_u8(1);
    WriteInner(w, *v);
  } else {
    w.write_u8(0);
  }
}

template <typename T, T (*ReadInner)(RustBufferReader &)>
inline std::optional<T> read_optional(RustBufferReader &r) {
  uint8_t tag = r.read_u8();
  if (tag == 0) {
    return std::nullopt;
  }
  return std::optional<T>{ReadInner(r)};
}

/// Top-level lower for `Option<T>`. Allocates a `RustBuffer`, writes the
/// tag + (maybe) payload, hands the buffer to Rust.
template <typename T, RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*Reserve)(RustBuffer, uint64_t, UniffiRustCallStatus *),
          void (*WriteInner)(Writer<Alloc, Reserve> &, const T &)>
inline RustBuffer lower_optional(const std::optional<T> &v) {
  Writer<Alloc, Reserve> w;
  write_optional<T, Alloc, Reserve, WriteInner>(w, v);
  return w.finish();
}

/// Top-level lift for `Option<T>`. Reads from a uniffi-returned buffer;
/// the caller is responsible for freeing the buffer after this returns.
template <typename T, T (*ReadInner)(RustBufferReader &)>
inline std::optional<T> lift_optional(RustBuffer buf) {
  RustBufferReader r{buf};
  return read_optional<T, ReadInner>(r);
}

// -----------------------------------------------------------------------
// Sequence<T> / Vec<T>.
// -----------------------------------------------------------------------

template <typename T, RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *),
          void (*WriteInner)(Writer<A, R> &, const T &)>
inline void write_sequence(Writer<A, R> &w, const std::vector<T> &v) {
  w.write_i32(static_cast<int32_t>(v.size()));
  for (const auto &item : v) {
    WriteInner(w, item);
  }
}

template <typename T, T (*ReadInner)(RustBufferReader &)>
inline std::vector<T> read_sequence(RustBufferReader &r) {
  int32_t len = r.read_i32();
  std::vector<T> out;
  if (len <= 0)
    return out;
  out.reserve(static_cast<size_t>(len));
  for (int32_t i = 0; i < len; ++i) {
    out.emplace_back(ReadInner(r));
  }
  return out;
}

template <typename T, RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*Reserve)(RustBuffer, uint64_t, UniffiRustCallStatus *),
          void (*WriteInner)(Writer<Alloc, Reserve> &, const T &)>
inline RustBuffer lower_sequence(const std::vector<T> &v) {
  Writer<Alloc, Reserve> w;
  write_sequence<T, Alloc, Reserve, WriteInner>(w, v);
  return w.finish();
}

template <typename T, T (*ReadInner)(RustBufferReader &)>
inline std::vector<T> lift_sequence(RustBuffer buf) {
  RustBufferReader r{buf};
  return read_sequence<T, ReadInner>(r);
}

// -----------------------------------------------------------------------
// Map<K, V> / HashMap<K, V>.
// -----------------------------------------------------------------------

template <typename K, typename V,
          RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *),
          void (*WriteK)(Writer<A, R> &, const K &),
          void (*WriteV)(Writer<A, R> &, const V &)>
inline void write_map(Writer<A, R> &w, const std::unordered_map<K, V> &m) {
  w.write_i32(static_cast<int32_t>(m.size()));
  for (const auto &kv : m) {
    WriteK(w, kv.first);
    WriteV(w, kv.second);
  }
}

template <typename K, typename V, K (*ReadK)(RustBufferReader &),
          V (*ReadV)(RustBufferReader &)>
inline std::unordered_map<K, V> read_map(RustBufferReader &r) {
  int32_t len = r.read_i32();
  std::unordered_map<K, V> out;
  if (len <= 0)
    return out;
  out.reserve(static_cast<size_t>(len));
  for (int32_t i = 0; i < len; ++i) {
    K k = ReadK(r);
    V v = ReadV(r);
    out.emplace(std::move(k), std::move(v));
  }
  return out;
}

template <typename K, typename V,
          RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*Reserve)(RustBuffer, uint64_t, UniffiRustCallStatus *),
          void (*WriteK)(Writer<Alloc, Reserve> &, const K &),
          void (*WriteV)(Writer<Alloc, Reserve> &, const V &)>
inline RustBuffer lower_map(const std::unordered_map<K, V> &m) {
  Writer<Alloc, Reserve> w;
  write_map<K, V, Alloc, Reserve, WriteK, WriteV>(w, m);
  return w.finish();
}

template <typename K, typename V, K (*ReadK)(RustBufferReader &),
          V (*ReadV)(RustBufferReader &)>
inline std::unordered_map<K, V> lift_map(RustBuffer buf) {
  RustBufferReader r{buf};
  return read_map<K, V, ReadK, ReadV>(r);
}

// -----------------------------------------------------------------------
// Bytes / Vec<u8>.
//
// Uniffi encodes `Vec<u8>` as length-prefixed raw bytes, *not* as a
// generic `Vec<u8>` (no per-element encoding). We model it as
// `std::vector<uint8_t>` on the C++ side.
// -----------------------------------------------------------------------

template <RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline void write_bytes(Writer<A, R> &w, const std::vector<uint8_t> &v) {
  w.write_i32(static_cast<int32_t>(v.size()));
  for (uint8_t b : v) {
    w.write_u8(b);
  }
}

inline std::vector<uint8_t> read_bytes(RustBufferReader &r) {
  int32_t len = r.read_i32();
  std::vector<uint8_t> out;
  if (len <= 0)
    return out;
  auto view = r.read_view(static_cast<size_t>(len));
  out.assign(view.data, view.data + view.len);
  return out;
}

template <RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*Reserve)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline RustBuffer lower_bytes(const std::vector<uint8_t> &v) {
  Writer<Alloc, Reserve> w;
  write_bytes<Alloc, Reserve>(w, v);
  return w.finish();
}

inline std::vector<uint8_t> lift_bytes(RustBuffer buf) {
  RustBufferReader r{buf};
  return read_bytes(r);
}

} // namespace ubrn::nitro
