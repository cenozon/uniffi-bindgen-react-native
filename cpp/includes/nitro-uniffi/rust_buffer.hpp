// SPDX-License-Identifier: MPL-2.0
//
// `RustBuffer` lifetime helper. The uniffi C ABI's `RustBuffer` is a
// (capacity, len, data*) triple that's *Rust-owned* — every alloc that
// comes back from `ffi_<crate>_rustbuffer_alloc` must be paired with a
// `ffi_<crate>_rustbuffer_free` on the same namespace, or Rust leaks the
// allocation.
//
// In the per-call generated method body it's tempting to manage this by
// hand, but every error path is a leak opportunity. `RustBufferOwned`
// gives RAII over the buffer: construct from a uniffi return value, let
// the destructor call the namespace-specific free hook. The generated code
// composes by:
//
//   RustBufferOwned<&ffi_my_crate_rustbuffer_free> buf{
//       uniffi_my_crate_fn_method_foo_bar(handle, lowered, &status)
//   };
//   check_status(status, free_my_crate_buf);
//   std::string out = read_string(buf.get());   // borrows from buf
//
// On the way *out* (lifting a std::string to RustBuffer to hand to Rust),
// the generated code uses `RustBufferWriter` (see writer.hpp), then
// `release()`s ownership so Rust sees the alloc.

#pragma once

#include <RustBuffer.h>
#include <UniffiRustCallStatus.h>

#include <chrono>
#include <cstdint>
#include <cstring>
#include <stdexcept>
#include <utility>

namespace ubrn::nitro {

/// A non-owning view over a RustBuffer's payload. Cheap to pass around;
/// callers read forward via `read_*` helpers below.
struct RustBufferView {
  const uint8_t *data;
  size_t len;
};

/// Position-tracked reader over a RustBuffer payload. Records every
/// `read_*` call by advancing the cursor; provides the de-facto
/// big-endian wire format uniffi uses for compound values (records,
/// enums, sequences). Big-endian because that's what uniffi's
/// `lower_into_buffer` uses on the Rust side.
class RustBufferReader {
public:
  explicit RustBufferReader(RustBufferView view) noexcept
      : data_(view.data), end_(view.data + view.len) {}

  explicit RustBufferReader(RustBuffer buf) noexcept
      : data_(buf.data), end_(buf.data + buf.len) {}

  bool eof() const noexcept { return data_ == end_; }

  uint8_t read_u8() {
    check_room(1);
    return *data_++;
  }

  int8_t read_i8() { return static_cast<int8_t>(read_u8()); }

  bool read_bool() { return read_u8() != 0; }

  uint16_t read_u16() {
    check_room(2);
    uint16_t v = (static_cast<uint16_t>(data_[0]) << 8) | data_[1];
    data_ += 2;
    return v;
  }

  int16_t read_i16() { return static_cast<int16_t>(read_u16()); }

  uint32_t read_u32() {
    check_room(4);
    uint32_t v = (static_cast<uint32_t>(data_[0]) << 24) |
                 (static_cast<uint32_t>(data_[1]) << 16) |
                 (static_cast<uint32_t>(data_[2]) << 8) |
                 static_cast<uint32_t>(data_[3]);
    data_ += 4;
    return v;
  }

  int32_t read_i32() { return static_cast<int32_t>(read_u32()); }

  uint64_t read_u64() {
    check_room(8);
    uint64_t v = 0;
    for (int i = 0; i < 8; ++i) {
      v = (v << 8) | data_[i];
    }
    data_ += 8;
    return v;
  }

  int64_t read_i64() { return static_cast<int64_t>(read_u64()); }

  float read_f32() {
    uint32_t bits = read_u32();
    float v;
    std::memcpy(&v, &bits, sizeof(v));
    return v;
  }

  double read_f64() {
    uint64_t bits = read_u64();
    double v;
    std::memcpy(&v, &bits, sizeof(v));
    return v;
  }

  /// Read a length-prefixed utf-8 byte sequence as std::string. Uniffi
  /// strings on the wire are an i32 length followed by `len` bytes.
  std::string read_string() {
    int32_t len = read_i32();
    if (len <= 0)
      return {};
    check_room(static_cast<size_t>(len));
    std::string out(reinterpret_cast<const char *>(data_),
                    static_cast<size_t>(len));
    data_ += len;
    return out;
  }

  /// Read raw bytes from the cursor as a span.
  RustBufferView read_view(size_t n) {
    check_room(n);
    RustBufferView v{data_, n};
    data_ += n;
    return v;
  }

  /// Read a uniffi-format SystemTime: i64 signed seconds offset from
  /// the Unix epoch + u32 subsecond nanos. The sign of `seconds`
  /// drives the direction of the offset (uniffi-rs stores the
  /// absolute value of seconds with a sign on the seconds field; the
  /// nanos field is always the non-negative subsecond magnitude).
  std::chrono::system_clock::time_point read_timestamp() {
    int64_t seconds = read_i64();
    uint32_t nanos = read_u32();
    // Match uniffi-rs's `try_read`: the absolute value of seconds is
    // the magnitude of the offset; the sign determines pre-/post-
    // epoch direction. nanos is always added (never subtracted).
    int64_t abs_seconds = seconds < 0 ? -seconds : seconds;
    auto offset =
        std::chrono::seconds(abs_seconds) + std::chrono::nanoseconds(nanos);
    auto epoch = std::chrono::system_clock::time_point{};
    if (seconds >= 0) {
      return epoch +
             std::chrono::duration_cast<std::chrono::system_clock::duration>(
                 offset);
    } else {
      return epoch -
             std::chrono::duration_cast<std::chrono::system_clock::duration>(
                 offset);
    }
  }

  /// Read a uniffi-format Duration: u64 magnitude seconds + u32 nanos.
  /// `nanos` is expected to be in [0, 999_999_999]. Returns the duration
  /// as a millisecond count (matches the TS/JSI backend's surface, which
  /// also exposes Duration as a `number` of milliseconds).
  double read_duration() {
    uint64_t seconds = read_u64();
    uint32_t nanos = read_u32();
    // seconds * 1000 + nanos / 1e6; do the multiply in double to
    // avoid integer overflow on multi-thousand-year durations and
    // keep submillisecond resolution as fractional ms.
    return static_cast<double>(seconds) * 1000.0 +
           static_cast<double>(nanos) / 1.0e6;
  }

private:
  void check_room(size_t want) const {
    if (data_ + want > end_) {
      throw std::runtime_error("RustBuffer underflow during read");
    }
  }

  const uint8_t *data_;
  const uint8_t *end_;
};

/// Buffer-builder for outbound RustBuffer payloads. The Rust side expects
/// the same big-endian wire format. Buffer growth uses `realloc`-like
/// semantics via the per-namespace `_reserve` hook, which the generated
/// code passes in. On `finish()`, ownership of the underlying alloc
/// transfers to the returned `RustBuffer` value — the Rust callee must
/// either consume it (in which case it frees on the Rust side) or hand it
/// back so we can free it.
template <RustBuffer (*Alloc)(uint64_t, UniffiRustCallStatus *),
          RustBuffer (*Reserve)(RustBuffer, uint64_t, UniffiRustCallStatus *)>
class RustBufferWriter {
public:
  RustBufferWriter() {
    UniffiRustCallStatus s{};
    buf_ = Alloc(64, &s);
    // Buffer allocation cannot fail under non-OOM conditions; uniffi's
    // allocator panics on OOM and aborts, so the status is "always
    // success" here — but we still bail loudly if the contract breaks.
    if (s.code != 0) {
      throw std::runtime_error("RustBuffer alloc failed");
    }
    buf_.len = 0;
  }

  void write_u8(uint8_t v) {
    ensure(1);
    buf_.data[buf_.len++] = v;
  }

  void write_i8(int8_t v) { write_u8(static_cast<uint8_t>(v)); }
  void write_bool(bool v) { write_u8(v ? 1 : 0); }

  void write_u16(uint16_t v) {
    ensure(2);
    buf_.data[buf_.len++] = static_cast<uint8_t>(v >> 8);
    buf_.data[buf_.len++] = static_cast<uint8_t>(v & 0xff);
  }

  void write_i16(int16_t v) { write_u16(static_cast<uint16_t>(v)); }

  void write_u32(uint32_t v) {
    ensure(4);
    buf_.data[buf_.len++] = static_cast<uint8_t>(v >> 24);
    buf_.data[buf_.len++] = static_cast<uint8_t>(v >> 16);
    buf_.data[buf_.len++] = static_cast<uint8_t>(v >> 8);
    buf_.data[buf_.len++] = static_cast<uint8_t>(v & 0xff);
  }

  void write_i32(int32_t v) { write_u32(static_cast<uint32_t>(v)); }

  void write_u64(uint64_t v) {
    ensure(8);
    for (int i = 7; i >= 0; --i) {
      buf_.data[buf_.len++] = static_cast<uint8_t>((v >> (i * 8)) & 0xff);
    }
  }

  void write_i64(int64_t v) { write_u64(static_cast<uint64_t>(v)); }

  void write_f32(float v) {
    uint32_t bits;
    std::memcpy(&bits, &v, sizeof(bits));
    write_u32(bits);
  }

  void write_f64(double v) {
    uint64_t bits;
    std::memcpy(&bits, &v, sizeof(bits));
    write_u64(bits);
  }

  /// Write a uniffi-format SystemTime: i64 signed seconds + u32 nanos.
  /// Mirrors uniffi-rs's `write` impl: split the time_point into a
  /// magnitude (absolute duration from the Unix epoch) and a sign
  /// (carried in the seconds field), then emit the absolute nanos as
  /// the subsecond component (uniffi-rs takes the subsec from the
  /// magnitude, not the signed delta).
  void write_timestamp(std::chrono::system_clock::time_point tp) {
    using namespace std::chrono;
    auto epoch = system_clock::time_point{};
    bool negative = tp < epoch;
    auto delta = negative ? epoch - tp : tp - epoch;
    auto secs = duration_cast<seconds>(delta);
    auto sub_ns = duration_cast<nanoseconds>(delta - secs);
    int64_t s = static_cast<int64_t>(secs.count());
    if (negative)
      s = -s;
    write_i64(s);
    write_u32(static_cast<uint32_t>(sub_ns.count()));
  }

  /// Write a uniffi-format Duration: u64 magnitude seconds + u32 nanos.
  /// The input is a millisecond count (matches the TS surface, which
  /// also passes Duration as a `number` of milliseconds). uniffi
  /// Durations are non-negative; negative inputs are clamped to zero
  /// to avoid a signed-cast UB on the u64.
  void write_duration(double ms) {
    if (ms < 0.0)
      ms = 0.0;
    // Split into integer seconds + remaining nanos. Trunc on ms to
    // avoid pulling a fractional second into the nanos slot via the
    // `% 1000` of a non-integer.
    uint64_t whole_ms = static_cast<uint64_t>(ms);
    uint64_t secs = whole_ms / 1000ULL;
    uint64_t rem_ms = whole_ms % 1000ULL;
    double frac_ms = ms - static_cast<double>(whole_ms);
    // rem_ms * 1e6 ns/ms + frac_ms * 1e6 ns/ms. The sum is < 1e9
    // (<1 second of nanos), so it fits a u32.
    uint32_t nanos = static_cast<uint32_t>(
        rem_ms * 1'000'000ULL + static_cast<uint64_t>(frac_ms * 1.0e6));
    write_u64(secs);
    write_u32(nanos);
  }

  /// Write a uniffi-format string: i32 length prefix then utf-8 bytes.
  void write_string(const std::string &s) {
    write_i32(static_cast<int32_t>(s.size()));
    ensure(s.size());
    std::memcpy(buf_.data + buf_.len, s.data(), s.size());
    buf_.len += s.size();
  }

  /// Hand ownership of the underlying RustBuffer to the caller. After
  /// this the writer is consumed and must not be used.
  RustBuffer finish() {
    RustBuffer out = buf_;
    buf_ = RustBuffer{};
    return out;
  }

private:
  void ensure(size_t want) {
    if (buf_.len + want <= buf_.capacity)
      return;
    UniffiRustCallStatus s{};
    buf_ = Reserve(buf_, static_cast<uint64_t>(want), &s);
    if (s.code != 0) {
      throw std::runtime_error("RustBuffer reserve failed");
    }
  }

  RustBuffer buf_;
};

} // namespace ubrn::nitro
