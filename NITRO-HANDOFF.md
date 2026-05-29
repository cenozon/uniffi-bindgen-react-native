# Nitro backend — handoff (heap-corruption bug, fresh eyes needed)

Branch `dev/km/find-2199-why-not`. Checkpoint committed at `5279593`
("feat(nitro): self-emit record/enum struct+JSIConverter+codec…").

## What's DONE and working (verified)

Phases A–B of the mission are essentially implemented. `gen_nitro` now
self-emits, with NO Nitrogen dependency for types:

- `templates/record.hpp` (new) — C++ struct + hand-written `JSIConverter`
  per uniffi record (mirrors Nitrogen's Car.hpp: PropNameIDCache /
  isPlainObject).
- `templates/enum.hpp` (new) — flat enum → `enum class` + string-union
  converter; tagged enum → per-variant payload structs in a `std::variant`
  wrapper + discriminated-union (`{type:'…', …}`) converter.
- `templates/codecs.hpp` — rewritten: stream `write_<Name>`/`read_<Name>`
  (uniform `(writer,value)`/`(reader)->value`) with forward decls for
  mutual recursion; `lower_`/`lift_` are thin wrappers. `codec_ready`
  deleted.
- `model.rs` — `write_/read_fn_template_arg` now return
  `&write_<Name>`/`&read_<Name>` for Record/Enum and
  `write_/read_interface_handle<…>` for Interface; `cxx_type` fully
  qualifies Record/Enum (`::margelo::nitro::<ns>::<Name>`) so names resolve
  inside the `margelo::nitro` JSIConverter namespace; Bytes →
  `std::shared_ptr<::margelo::nitro::ArrayBuffer>` (TS `ArrayBuffer`);
  `dependency_headers()` / `api_dependency_headers()`;
  `primary_constructor()`; `returns_owned_rustbuffer()`.
- `cpp/includes/nitro-uniffi/composites.hpp` — interface-handle thunks;
  ArrayBuffer-based bytes codec (single memcpy each way); primitive write
  thunks changed to `const T&` so they match the composite
  function-pointer signature.
- `cpp/includes/nitro-uniffi/rust_buffer.hpp` — `write_raw_bytes`.
- `cpp/includes/nitro-uniffi/jsi_converter_ints.hpp` (new) — JSIConverter
  for u8/i8/u16/i16/u32 (Nitro core only ships int/int64/uint64/double/
  float/bool/string). int32==int is intentionally NOT respecialized.
- `interface.{hpp,cpp}` — argless uniffi constructor wired into the C++
  default ctor (`make_handle()`), so `new Counter()` → live Rust object.
- `namespace.ts` — exports records/enums + a constructable class shim per
  interface.
- All four TUs syntax-check; the full benchmark Nitro build compiles,
  bundles (tsc passes), and RUNS.

### Proven correct in isolation (all ASAN-clean)
- The RustBuffer record codec: `lift_LargeRecord`/`lower_LargeRecord`
  round-trips perfectly (`/tmp/REPRO_codec.cpp`, `/tmp/REPRO_writer.cpp`).
- `lift_bytes` (ArrayBuffer::copy) 2000× (`/tmp/REPRO_arraybuffer.cpp`).
- Writer→Rust-free roundtrip 500× at 1MB (correct capacity 1048580).
- `getLargeRecord` 30000× standalone (glibc, no ASAN) — clean.
- uniffi `Vec<u8>` wire format confirmed: i32 count + raw bytes (matches
  our `write_bytes`). Record field order matches metadata.

## The REMAINING bug

`cargo test -p uniffi-fixture-benchmark -- nitro` runs all the early
benchmarks fine (string/bytes/noop/addU32/**Counter.increment works**),
then aborts with **`malloc(): unaligned tcache chunk detected`** at the
FIRST `getLargeRecord()` after the Counter.increment loop.

Backtrace shows the abort is inside
`uniffi_uniffi_benchmark_fn_func_get_large_record` doing a normal `malloc`
— i.e. **the heap was already corrupted earlier; get_large_record's malloc
is just the detector.** It's a shared glibc heap (Rust + Hermes + Nitro +
our C++ all use it).

### Repro harness (fast, no cargo)
`/tmp/REPRO_HARNESS.cpp` is a minimal Hermes+Nitro JSI harness. Build+run
recipe (the `js` body in the file drives it):

```
REPO=/home/agent-grant/dev/uniffi-bindgen-react-native
NCPP=$REPO/node_modules/react-native-nitro-modules/cpp
HSRC=$REPO/cpp_modules/hermes
GEN=$REPO/fixtures/benchmark/generated/nitro/cpp   # regenerate first if templates changed
clang++ -std=c++20 -O0 -g -rdynamic -o /tmp/repro /tmp/REPRO_HARNESS.cpp \
  $GEN/HybridUniffiBenchmarkApi.cpp $GEN/HybridCounter.cpp $GEN/register_natives.cpp \
  -I$GEN -I$REPO/cpp/test-harness \
  -I"$HSRC/API" -I"$HSRC/API/jsi" -I"$HSRC/public" \
  -I"$NCPP" -I"$NCPP/core" -I"$NCPP/jsi" -I"$NCPP/entrypoint" -I"$NCPP/platform" \
  -I"$NCPP/prototype" -I"$NCPP/registry" -I"$NCPP/templates" -I"$NCPP/threading" \
  -I"$NCPP/utils" -I"$NCPP/views" \
  -I"$REPO/build/nitro-build/include" -I"$REPO/cpp/includes" -I"$REPO/cpp/stubs" \
  -L"$REPO/target/debug" -luniffi_benchmark -L"$REPO/build/nitro-build" -lNitroModules \
  -L"$REPO/build/hermes/lib" -lhermesvm -L"$REPO/build/hermes/jsi" -ljsi \
  -Wl,-rpath,"$REPO/target/debug" -Wl,-rpath,"$REPO/build/nitro-build" \
  -Wl,-rpath,"$REPO/build/hermes/lib" -Wl,-rpath,"$REPO/build/hermes/jsi"
/tmp/repro
```
(To regenerate `$GEN`: write a throwaway `#[test]` calling
`BindingsArgs::run` with `AbiFlavor::Nitro` against
`target/debug/libuniffi_benchmark.so` — see the deleted
`crates/ubrn_bindgen/tests/zz_tmp_emit_benchmark.rs` in git reflog/history,
or `cargo test -p uniffi-fixture-benchmark -- nitro` which regenerates into
`fixtures/benchmark/generated/nitro/cpp`.)

### KEY CLUES (what's been ruled in/out)
1. **Bisect: the corruptor is the combination `Counter` + byte/string
   ArrayBuffer churn, at scale (300000 increments + ~1200 1MB buffer ops).**
   - Remove the `Counter` create+increment lines from the harness body →
     NO crash. Counter is necessary.
   - `Counter.increment` 300000× ALONE → no crash, but returns the WRONG
     value: deterministic **59895** instead of 300000 under glibc; correct
     300000 under ASAN. Ramp test: 100/1k/60k/100k increments all return
     the correct value; only 300000 diverges. **Non-deterministic across
     builds (saw 59895, 126062) → reading corrupted/garbage memory.**
   - Each partial sub-sequence (getBytes×600, takeBytes×600, getString×600,
     Counter×300k, noop×300k+Counter, getBytes+Counter) individually does
     NOT crash. Only the FULL benchmark prefix does. Classic
     small-OOB-write that lands in slack until enough churn corrupts a
     chunk header.
2. **ASAN HIDES it** (redzones absorb the OOB) → it's a real small
   out-of-bounds write or a bad free with slightly-wrong size, not a
   logic-only bug.
3. **`getBytes(1MB)` is pathologically slow: ~27ms (512KB) / ~53ms (1MB)
   per call** vs getString ~0.27/0.48ms. That's ~100× too slow for a
   single 1MB memcpy. Strong hint the ArrayBuffer return path (NativeArray
   Buffer → JS, possibly a finalizer/GC interaction in Hermes, or a hidden
   re-copy) is misbehaving — and is the most likely corruption source.
   **Prime suspect: how we return `std::shared_ptr<ArrayBuffer>`
   (NativeArrayBuffer made by `ArrayBuffer::copy`) to JS via Nitro's
   `JSIConverter<shared_ptr<ArrayBuffer>>`, and/or how Hermes frees that
   external buffer.** Compare against how nitrogen/Nitro's own tests return
   ArrayBuffers (look in `/home/agent-grant/dev/nitro/packages/
   react-native-nitro-test/nitrogen/generated/**`).

### Suggested next steps for fresh eyes
- First: confirm whether reverting Bytes from `ArrayBuffer` back to a
  plain representation makes the corruption AND the 53ms both disappear.
  Quick way: in `model.rs` temporarily map `Bytes` cxx_type to
  `std::vector<uint8_t>` + ts_type `number[]`, give composites.hpp a
  vector-based `write/read/lift/lower_bytes`, and change the benchmark TS
  test's `takeBytes` args from `new ArrayBuffer(...)` to `new
  Uint8Array(...)` / a number[]. If corruption vanishes → it's the
  ArrayBuffer path; fix THAT (correct NativeArrayBuffer ownership/return)
  and keep ArrayBuffer (it's the desired perf path, §2.3). If corruption
  persists → look elsewhere (the Counter handle path, `make_status`, the
  writer at small sizes).
- The 53ms/call getBytes is independently a red flag worth fixing
  regardless — chase it; it likely shares a root cause with the
  corruption.
- Consider building the actual `test-runner-nitro` with `-fsanitize=address`
  (rebuild the per-fixture .so + runner with ASAN via the cmake in
  `crates/ubrn_fixture_testing/src/nitro.rs` / `cpp/test-harness/`) and run
  the real bundle — ASAN won't reproduce the glibc abort but WILL flag the
  underlying OOB write/bad-free with a precise stack.

## Still TODO after the bug (mission Phases C–D)
- Audit every `NitroType` in every position (incl. interface-in-composite,
  callback-in-composite) for gaps.
- Tagged-enum flat-enum string values now use lowerCamel `tag` (self
  consistent C++↔TS). Confirm arithmetic/coverall/futures/callbacks Nitro
  tests + `cargo test -p ubrn_cli` still green; update assertions if
  emission legitimately changed.
- Phase D: move autolinking (gradle/cmake/OnLoad/podspec/+load) into our
  templates and remove `run_nitrogen` from `ubrn_cli` generate/jsi flow;
  update ubrn_cli codegen tests.
- Final gates: `cargo fmt --all --check`; clippy `-Dwarnings`; `xtask fmt
  --check typescript` and `--check cpp`.
- `CLAUDE-TODO.md` is untracked (leave as-is). `NITRO-HANDOFF.md` (this
  file) can be deleted once resolved.
