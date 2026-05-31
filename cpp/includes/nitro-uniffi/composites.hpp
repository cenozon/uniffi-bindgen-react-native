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

#include <memory>

#include <NitroModules/ArrayBuffer.hpp>

#include "converters.hpp"
#include "handle.hpp"
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

// The `const T&` parameter spelling is load-bearing: the composite thunks
// (`write_optional`, `write_sequence`, `write_map`) declare their inner
// writer as `void (*)(Writer<A, R>&, const T&)`, so a by-value primitive
// writer would be a different function-pointer type and fail to match as a
// template argument. Taking `const T&` keeps every writer thunk uniform.
#define UBRN_NITRO_PRIM_THUNK(T, suffix)                                       \
  template <RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),                 \
            RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>     \
  inline void write_##suffix(Writer<A, R> &w, const T &v) {                    \
    w.write_##suffix(v);                                                       \
  }                                                                            \
  inline T read_##suffix(RustBufferReader &r) { return r.read_##suffix(); }

// Writer-type-generic primitive write thunks. Keyed on the writer type `W`
// (any `RustBufferWriter<A, R>` instantiation) rather than on the
// `Alloc`/`Reserve` symbol pair. The per-type record / enum stream codecs in
// `<namespace>_codecs.hpp` are themselves templated on the writer type so a
// record from namespace A can be serialized straight into namespace B's
// outer buffer (a cross-namespace field); these thunks let those codec
// bodies spell their primitive field writers as `write_<suffix><W>(w, v)`
// without pinning the foreign namespace's allocator symbols. Named
// `write_<suffix>_w` to coexist unambiguously with the symbol-keyed thunks
// above (which the top-level `lower_*` path still uses).
#define UBRN_NITRO_PRIM_THUNK_W(T, suffix)                                     \
  template <typename W> inline void write_##suffix##_w(W &w, const T &v) {     \
    w.write_##suffix(v);                                                       \
  }

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

UBRN_NITRO_PRIM_THUNK_W(uint8_t, u8)
UBRN_NITRO_PRIM_THUNK_W(uint16_t, u16)
UBRN_NITRO_PRIM_THUNK_W(uint32_t, u32)
UBRN_NITRO_PRIM_THUNK_W(uint64_t, u64)
UBRN_NITRO_PRIM_THUNK_W(int8_t, i8)
UBRN_NITRO_PRIM_THUNK_W(int16_t, i16)
UBRN_NITRO_PRIM_THUNK_W(int32_t, i32)
UBRN_NITRO_PRIM_THUNK_W(int64_t, i64)
UBRN_NITRO_PRIM_THUNK_W(float, f32)
UBRN_NITRO_PRIM_THUNK_W(double, f64)
UBRN_NITRO_PRIM_THUNK_W(bool, bool)

#undef UBRN_NITRO_PRIM_THUNK
#undef UBRN_NITRO_PRIM_THUNK_W

// std::string thunks — wire-encoded as i32 length + utf-8 bytes.
template <RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline void write_string(Writer<A, R> &w, const std::string &s) {
  w.write_string(s);
}

inline std::string read_string(RustBufferReader &r) { return r.read_string(); }

// Writer-type-generic string/date/duration thunks (see the `_w` primitive
// note above): used inside the writer-templated record / enum stream codecs.
template <typename W> inline void write_string_w(W &w, const std::string &s) {
  w.write_string(s);
}

// Timestamp / SystemTime thunks — wire-encoded as i64 seconds + u32 nanos.
template <RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline void write_timestamp(Writer<A, R> &w,
                            const std::chrono::system_clock::time_point &tp) {
  w.write_timestamp(tp);
}

template <typename W>
inline void write_timestamp_w(W &w,
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

template <typename W> inline void write_duration_w(W &w, const double &ms) {
  w.write_duration(ms);
}

inline double read_duration(RustBufferReader &r) { return r.read_duration(); }

// -----------------------------------------------------------------------
// Interface handles inside a composite.
//
// A uniffi `interface` crosses the C ABI as a bare `uint64_t` Arc handle.
// When one appears *inside* a composite (`Vec<Counter>`, `Option<Counter>`,
// a record field, an enum payload) it's wire-encoded as that u64. The
// write thunk clones the handle off the `std::shared_ptr<HybridT>`; the
// read thunk wraps the decoded handle back into a fresh `HybridT`.
//
// Lowering MUST clone: when uniffi reads an interface handle out of a
// buffer it lifts it with `Arc::from_raw` and drops it on return — i.e. it
// consumes one Arc reference. Handing over a clone (via `clone_handle()`,
// which bumps the Rust-side strong count) keeps the `shared_ptr` the JS
// side holds valid.
// -----------------------------------------------------------------------

template <typename HybridT, RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline void write_interface_handle(Writer<A, R> &w,
                                   const std::shared_ptr<HybridT> &v) {
  w.write_u64(v->clone_handle());
}

// Writer-type-generic interface-handle write thunk for the writer-templated
// record / enum stream codecs.
template <typename HybridT, typename W>
inline void write_interface_handle_w(W &w, const std::shared_ptr<HybridT> &v) {
  w.write_u64(v->clone_handle());
}

template <typename HybridT>
inline std::shared_ptr<HybridT> read_interface_handle(RustBufferReader &r) {
  // The decoded u64 is an OWNED Arc handle (uniffi `Arc::into_raw`). Route it
  // through the generated `Hybrid<Name>::adopt` choke point, which parks the
  // handle in a move-only guard THROUGH the allocation and only relinquishes it
  // into the object after it exists — so a throwing allocation / Nitro base
  // ctor frees the handle on unwind instead of leaking the strong count (audit
  // bug #27). A bare `make_shared<HybridT>(raw)` had no such guard window.
  return HybridT::adopt(r.read_u64());
}

// -----------------------------------------------------------------------
// Callback interfaces inside a composite.
//
// A uniffi callback interface / `with_foreign` trait object also crosses
// the C ABI as a bare `uint64_t` handle, but the two directions differ
// from a plain interface:
//
//   * WRITE (us -> Rust): the value is a `std::shared_ptr<Hybrid<Name>>`
//     that is either a JS-implemented instance (must be registered with the
//     per-type `CallbackHandleMap` after its vtable is installed) or a
//     Rust-backed proxy (its existing Rust handle must be cloned). Both
//     cases — plus the per-callback vtable install, which a generic thunk
//     can't name — are funnelled through the generated `Hybrid<Name>`
//     class's `static ensure_vtable()` + instance `lower_to_handle(self)`
//     surface (see `templates/callback.{hpp,cpp}`). `self` is passed as the
//     `shared_ptr` so the handle map takes shared ownership of the JS impl.
//
//   * READ (Rust -> us): Rust embedded an `Arc<dyn Trait>` as a handle;
//     wrap it in a Rust-backed proxy `Hybrid<Name>` via the `FromRustHandle`
//     ctor (mirrors the top-level `lift_expr` callback path).
// -----------------------------------------------------------------------

template <typename HybridT, RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline void write_callback_handle(Writer<A, R> &w,
                                  const std::shared_ptr<HybridT> &v) {
  HybridT::ensure_vtable();
  w.write_u64(v->lower_to_handle(v));
}

// Writer-type-generic callback-handle write thunk for the writer-templated
// record / enum stream codecs.
template <typename HybridT, typename W>
inline void write_callback_handle_w(W &w, const std::shared_ptr<HybridT> &v) {
  HybridT::ensure_vtable();
  w.write_u64(v->lower_to_handle(v));
}

template <typename HybridT>
inline std::shared_ptr<HybridT> read_callback_proxy(RustBufferReader &r) {
  // The decoded u64 is an OWNED `Arc<dyn Trait>` handle. Route it through the
  // generated proxy `Hybrid<Name>::adopt` choke point — same exception-safe
  // guard dance as `read_interface_handle`, so a throwing allocation / Nitro
  // base ctor frees the handle on unwind rather than leaking it (audit bug #27).
  return HybridT::adopt(r.read_u64());
}

// -----------------------------------------------------------------------
// Fallback for any uniffi type the Nitro backend doesn't model in
// composite-element position. `from_type` is total over uniffi's type
// universe, so these are never instantiated in practice — but referencing
// a defined symbol keeps a hypothetical `Stub` field from producing an
// opaque link error instead of a clear runtime throw.
// -----------------------------------------------------------------------

template <typename W, typename T>
inline void unsupported_compound_inside_composite(W &, const T &) {
  throw std::runtime_error(
      "Nitro: unsupported uniffi type in composite-element position");
}

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

// Writer-type-generic `Option<T>` writer. Keyed on the writer type `W` (so a
// record / enum codec — itself templated on `W` — can hold optional fields
// of any element type, including a cross-namespace record/enum whose `_w`
// element thunk is also `W`-keyed) instead of the `Alloc`/`Reserve` symbol
// pair. Used only inside `<namespace>_codecs.hpp` stream codecs.
template <typename T, typename W, void (*WriteInner)(W &, const T &)>
inline void write_optional_w(W &w, const std::optional<T> &v) {
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

template <typename T>
inline size_t seq_body_size_hint(const std::vector<T> &, size_t) { return 0; }
inline size_t seq_body_size_hint(const std::vector<std::string> &v, size_t) {
  size_t total = 0;
  for (const auto &s : v) total += 4 + s.size();
  return total;
}

template <typename T, RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *),
          void (*WriteInner)(Writer<A, R> &, const T &)>
inline void write_sequence(Writer<A, R> &w, const std::vector<T> &v) {
  w.write_i32(static_cast<int32_t>(v.size()));
  w.reserve_additional(seq_body_size_hint(v, 0));
  for (const auto &item : v) {
    WriteInner(w, item);
  }
}

// Writer-type-generic `Vec<T>` writer (see `write_optional_w`).
template <typename T, typename W, void (*WriteInner)(W &, const T &)>
inline void write_sequence_w(W &w, const std::vector<T> &v) {
  w.write_i32(static_cast<int32_t>(v.size()));
  w.reserve_additional(seq_body_size_hint(v, 0));
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

// Writer-type-generic `HashMap<K, V>` writer (see `write_optional_w`).
template <typename K, typename V, typename W,
          void (*WriteK)(W &, const K &), void (*WriteV)(W &, const V &)>
inline void write_map_w(W &w, const std::unordered_map<K, V> &m) {
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
// Uniffi encodes `Vec<u8>` as a length-prefixed raw byte run (i32 length
// then `len` bytes — no per-element encoding). The C++ surface is Nitro's
// `std::shared_ptr<ArrayBuffer>`, which JS sees directly as a JS
// `ArrayBuffer`: zero-copy on the read side (JS reads the native buffer in
// place) and a single copy on each direction's native boundary (vs the two
// copies a `Uint8Array` round-trip would cost). See CLAUDE-TODO §2.3.
//
// `read_bytes` copies the wire bytes once into a fresh owning ArrayBuffer.
// `write_bytes` copies the ArrayBuffer's bytes once into the RustBuffer.
// -----------------------------------------------------------------------

using BytesT = std::shared_ptr<::margelo::nitro::ArrayBuffer>;

template <RustBuffer (*A)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*R)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline void write_bytes(Writer<A, R> &w, const BytesT &v) {
  size_t n = (v == nullptr) ? 0 : v->size();
  w.write_i32(static_cast<int32_t>(n));
  if (n > 0) {
    w.reserve_additional(n);
    w.write_raw_bytes(v->data(), n);
  }
}

// Writer-type-generic `Vec<u8>` writer for the writer-templated stream codecs.
template <typename W> inline void write_bytes_w(W &w, const BytesT &v) {
  size_t n = (v == nullptr) ? 0 : v->size();
  w.write_i32(static_cast<int32_t>(n));
  if (n > 0) {
    w.reserve_additional(n);
    w.write_raw_bytes(v->data(), n);
  }
}

inline BytesT read_bytes(RustBufferReader &r) {
  int32_t len = r.read_i32();
  if (len <= 0) {
    return ::margelo::nitro::ArrayBuffer::allocate(0);
  }
  auto view = r.read_view(static_cast<size_t>(len));
  return ::margelo::nitro::ArrayBuffer::copy(view.data, view.len);
}

template <RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*Reserve)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
inline RustBuffer lower_bytes(const BytesT &v) {
  Writer<Alloc, Reserve> w;
  write_bytes<Alloc, Reserve>(w, v);
  return w.finish();
}

inline BytesT lift_bytes(RustBuffer buf) {
  RustBufferReader r{buf};
  return read_bytes(r);
}

/// Zero-copy top-level lift for a `Vec<u8>` return. The uniffi wire payload
/// is `[i32 len][len raw bytes]`; instead of copying the bytes out (as
/// `lift_bytes` does for the in-composite case, where the reader is shared),
/// we hand the payload region *directly* to JS as the ArrayBuffer's backing
/// store. The whole Rust-owned `RustBuffer` is reclaimed via `FreeFn` when
/// JS garbage-collects the ArrayBuffer — Hermes invokes the `wrap` deleter
/// from the external-buffer finalizer. One Rust allocation, zero copies on
/// the lift. Only valid when we own the entire buffer (a direct return /
/// out value), which is why it is distinct from the copying `read_bytes`.
template <void (*FreeFn)(RustBuffer, UniffiRustCallStatus *)>
inline BytesT lift_bytes_owning(RustBuffer buf) {
  RustBufferReader r{buf};
  int32_t len = r.read_i32();
  if (len <= 0) {
    UniffiRustCallStatus s{};
    FreeFn(buf, &s);
    return ::margelo::nitro::ArrayBuffer::allocate(0);
  }
  // Payload starts after the 4-byte big-endian length prefix.
  uint8_t *payload = buf.data + 4;
  return ::margelo::nitro::ArrayBuffer::wrap(
      payload, static_cast<size_t>(len), [buf]() {
        UniffiRustCallStatus s{};
        FreeFn(buf, &s);
      });
}

} // namespace ubrn::nitro
