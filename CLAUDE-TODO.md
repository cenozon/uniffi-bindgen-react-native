# Outstanding work — ubrn Nitro backend

Working document tracking what's left on `dev/km/find-2199-why-not` and the
follow-on perf work. Rough execution order.

---

## 0. In-flight: refactor + push (BLOCKED on CI fmt failures)

CI sweep result on dev/km/find-2199-why-not @ 8bdd1a5:

- ❌ `cargo xtask fmt --check rust`
- ❌ `cargo xtask fmt --check typescript`
- ✅ build, unit, bins, jsi, napi, nitro, wasm, napi-npm, checkout

Fix the two fmt failures, land the reorg below in the same amend, re-run the
full 11-step sweep, force-push.

---

## 1. Reorg — get Nitro out of `runtimes/`

`runtimes/nitro/` is misnamed and misplaced. It's not a runtime — it's a
header-only support library plus a desktop test stub. The JSI flavor's
equivalent lives at `cpp/includes/`, not `runtimes/jsi/`. Match that
convention.

### Moves

| From | To |
|--|--|
| `runtimes/nitro/cpp/NitroUniffi.hpp` | `cpp/includes/NitroUniffi.hpp` |
| `runtimes/nitro/cpp/nitro-uniffi/*.hpp` | `cpp/includes/nitro-uniffi/*.hpp` |
| `runtimes/nitro/cpp/platform-desktop/ThreadUtils.cpp` | `cpp/test-harness/platform-host/ThreadUtils.cpp` |
| `cpp/test-harness-nitro/test-runner-nitro.cpp` | `cpp/test-harness/test-runner-nitro.cpp` |

The `.cpp` stub belongs with its only consumer (the test harness), not in
the include tree. Rename `desktop` → `host` in source comments and the stub
class name — "desktop" overloads with RN-on-Desktop (react-native-windows /
react-native-macos), which this isn't. The bare-Hermes CLI process runs on
the host OS, hence `host`.

### Deletes (after moves land)

- `runtimes/nitro/` (typescript/ subdir, package.json, README.md, now-empty cpp/)
- `cpp/test-harness-nitro/`

### Consolidate CMakeLists

`cpp/test-harness/CMakeLists.txt` builds two binaries from the shared
sources: `test-runner` (JSI) and `test-runner-nitro` (Nitro). They share
`MyCallInvoker.h` and `timers.js.inc` already. The `xtask bootstrap`
sub-steps stay separate (`test-runner` and `test-runner-nitro` are two
deliverables) but both invoke the same cmake target dir with a different
`--target` argument.

### Path-reference rewires

- `xtask/src/bootstrap/nitro.rs` — CMakeLists generation references
  `runtimes/nitro/cpp/platform-desktop/ThreadUtils.cpp` (now
  `cpp/test-harness/platform-host/ThreadUtils.cpp`); the libNitroModules
  build still needs to compile that source in
- `xtask/src/bootstrap/nitro_test_runner.rs` — likely merges into
  `xtask/src/bootstrap/test_runner.rs` (both build cmake targets in
  `cpp/test-harness/`)
- `crates/ubrn_fixture_testing/src/paths.rs` — `nitro_flat_include_dir`,
  `ubrn_nitro_runtime_pkg_dir`, anything pointing under `runtimes/nitro`
- `crates/ubrn_fixture_testing/src/nitro.rs` — cmake include paths in
  `write_cmake_lists`
- Source comments in `crates/ubrn_bindgen/src/bindings/gen_nitro/*.rs`
  that name `runtimes/nitro` or `desktop`

Re-run the full 11-step CI sweep before push.

---

## 2. Performance wins — NO uniffi-rs fork required

These land purely in `gen_nitro` emission + `cpp/includes/nitro-uniffi/`
runtime headers. ROI order.

### 2.1 Waker-based async (kill the Nitro worker thread)

Current shape (`namespace_api.cpp` template):

```cpp
return Promise<T>::async([handle]() -> T {
  drive_rust_future(handle, &poll_symbol);  // worker thread spins on condvar
  return lift(complete_symbol(handle, &status));
});
```

Three thread hops + a worker thread holding a condvar for the entire
duration of the Rust future.

Target:

```cpp
return Promise<T>::create([handle](resolve, reject) {
  register_uniffi_waker(handle, &poll_symbol,
    [handle, resolve, reject]() {
      Dispatcher::getRuntimeGlobalDispatcher().runAsync([...] {
        auto raw = complete_symbol(handle, &status);
        free_symbol(handle);
        if (status.code == 0) resolve(lift(raw));
        else reject(decode_error(status));
      });
    });
});
```

One real thread hop (Dispatcher → JS). No worker thread. uniffi's existing
poll-callback C ABI is reused as-is; we just stop blocking on it.

**Touches:**
- `cpp/includes/nitro-uniffi/future.hpp` — replace blocking
  `drive_rust_future` with callback-based `register_uniffi_waker`
- `gen_nitro/templates/namespace_api.cpp` + `interface.cpp` async arms —
  emit `Promise<T>::create([](resolve, reject) { ... })` shape
- Possibly `gen_nitro/templates/callback.cpp` for callback methods that
  return Promises

### 2.2 `[Throws=None]` fast path

Skip the `try / check_status / catch / decode` block when uniffi metadata
says the method can't throw. Body becomes:

```cpp
auto raw = uniffi_symbol(lowered, &status);
// no post-call branch
return lift(raw);
```

The `RustCallStatus` out-param is still passed (the C ABI requires it),
but the entire decode/throw path disappears.

**Touches:**
- `gen_nitro/model.rs` — `NitroFunction.throws` is already `Option`; just
  gate template emission on Some/None
- `gen_nitro/templates/{namespace_api,interface}.cpp` — two variants

### 2.3 `Vec<u8>` / `String` via NitroArrayBuffer

Current: uniffi returns `RustBuffer`; we copy into a JS `Uint8Array` /
utf-8 string. **Two copies per round-trip** (Rust→buffer + buffer→JS heap).

Target: copy once into a `NitroArrayBuffer`; JS sees it directly via the
`ArrayBuffer` interface, zero-copy on the read side.

- **Lift:** alloc `NitroArrayBuffer` of `buf.len`, memcpy from `buf.data`,
  return to JS as `ArrayBuffer`
- **Lower:** alloc Nitro-owned buffer, JS writes into it, hand bytes to
  uniffi as `RustBuffer{data: nitro.data(), len: nitro.size()}`; Rust
  copies once on its end

Net: 2 copies → 1 copy. ace's `serialize()` / `to_bytes()` paths get this.

**Touches:**
- `cpp/includes/nitro-uniffi/converters.hpp` — `lift_string` /
  `lift_bytes` / `lower_bytes`
- `gen_nitro/model.rs` — `Bytes` ts_type becomes `ArrayBuffer` instead of
  `Uint8Array`

### 2.4 `Vec<T>` of primitives → typed ArrayBuffer

Same intercept point as 2.3. For `Vec<int32_t>`, decode the RustBuffer's
elements into a Nitro-owned buffer, hand JS an `Int32Array` view directly.

**Endianness wart:** uniffi writes big-endian into the RustBuffer; mobile
is little-endian. Per-element byte-swap on the C++ side. Still better than
per-element JSI value conversion.

**Touches:** `gen_nitro/model.rs` `Sequence(_)` arms for primitive T;
runtime helpers in `nitro-uniffi/composites.hpp`.

---

## 3. Performance wins — uniffi-rs fork required

Use the existing celestra uniffi-rs fork as the maintenance vehicle.
These need Rust-side scaffold changes that can't be done from the C++
side alone.

### 3.1 Records-of-primitives via `#[repr(C)]` — BIGGEST sync win

Patch points in the fork:

1. **`uniffi_bindgen` metadata pass:** tag each `Record` as C-ABI-eligible
   iff every field is itself C-ABI-eligible. Eligibility is transitive
   (primitives, handles, other C-ABI-eligible records, `#[repr(C)]`-eligible
   enums all count). String/Vec/HashMap fields break eligibility.
2. **`uniffi_macros::expand_record` + scaffold backend:** for eligible
   records emit
   ```rust
   #[repr(C)] pub struct CRepr_Foo { /* fields in metadata order */ }
   impl From<CRepr_Foo> for Foo { ... }
   impl From<Foo> for CRepr_Foo { ... }
   ```
   and rewrite every `extern "C"` signature taking or returning `Foo` to
   use `CRepr_Foo` instead of `RustBuffer`.
3. **gen_nitro side (this repo's fork):** when metadata flags a record as
   C-ABI-eligible, emit a matching C++ `extern "C" struct` with identical
   field order/types. Lift becomes `return arg;` (already the right
   shape); lower same. **Skip codec emission entirely for these types.**

Layout gotchas:
- Field order driven by metadata, not source
- `bool` is `int8_t` on uniffi's ABI; preserve
- Nested non-eligible records break transitivity
- Optional fields → fall back to buffer path (sentinel discriminant
  otherwise; not worth it)

**Win on ace:** vast majority of records-of-primitives go from
encode-into-buffer / decode-out-of-buffer to pass-by-value. Biggest
single per-call overhead being eliminated.

### 3.2 Typed `rust_future_complete_<T>` (compl. to 2.1)

Today `rust_future_complete_<T>` returns `RustBuffer` for compound `T`.
Patch the future driver to return `#[repr(C)] CRepr_<T>` for eligible
types. Eliminates the encode + decode round-trip on every awaited call
that returns a record.

**Touches:** `uniffi_core::rust_future` + scaffold's per-method async
emission. Compose with 2.1 — the waker-based path calls
`complete_symbol` once and gets a typed struct back.

### 3.3 `register_waker` C ABI (compl. to 2.1)

For 2.1 to be fully optimal, replace the multi-shot poll-callback dance
with a single-shot `register_waker(handle, fn, data)`. Cleaner generated
code, fewer Dispatcher hops on futures that complete after multiple
poll-pending cycles.

**Touches:** `uniffi_core::rust_future` C ABI.

### 3.4 Tagged-union enums

Only for enums whose every variant is `#[repr(C)]`-eligible (no
String/Vec/HashMap payloads). Emit a C tagged union:

```c
struct CRepr_E {
  int32_t tag;
  union {
    struct { int32_t v; } A;
    struct { /* ... */ } B;
  } payload;
};
```

Both sides match layout. Niche — measure on ace before committing.

---

## 4. Open architectural questions

### 4.1 `com.margelo.nitro.*` Android namespace

Nitrogen hardcodes `com.margelo.nitro.<crate>` as the prefix for
autolinking-generated Kotlin/Java. Nothing in `nitro.json#androidNamespace`
overrides it — it's a literal in Nitrogen's templates.

Options:
- (a) Accept the upstream convention (current state)
- (b) Fork Nitrogen to allow a custom prefix
  (e.g. `com.cenozon.nitro.<crate>`)

**Decision deferred.**

### 4.2 Optional<Timestamp> / Optional<Duration> codec_ready

`Lifespan { validTo: Optional<Timestamp> }` flips the record's
`codec_ready` flag to false. Same pre-existing limitation for any
`Optional` / `Sequence` / `Map` field inside a record — the in-record
codec writer doesn't know how to recurse those composites.

Not a regression from the all-types pass; was always like this. Fix:
extend the codec writer to handle nested composites.

**Touches:** `gen_nitro/templates/codecs.hpp` template — generate the
recursive Option/Sequence/Map read/write inside the record field walk.

### 4.3 Forked-Nitrogen vs accept-upstream

Folded into 4.1.

---

## 5. Indoraptor side

Branch `dev/km/find-2199-nitro-expo` on `cenozon/indoraptor` (sha
d64f7e3d9):

- Both `ace-db-expo` and `indoraptor-portal-v1-expo` flipped to
  `framework: nitro`
- No PR opened — gated on the ubrn-fork push landing

Once ubrn lands cleanly:
- Confirm `pnpm ubrn:android --and-generate` for ace produces output
  identical (or close enough) to the local v3 emission
- Same for ios
- Run the postinstall flow on a sample consumer (e.g. find-app) to
  validate the autolinking actually picks up the Nitro HybridObjects

---

## 6. Minor cleanups (anytime)

- `register_natives.cpp` template still includes
  `<ReactCommon/CallInvoker.h>` even though the param is gone — drop the
  include
- `RustBufferOwned` documented in `rust_buffer.hpp` doesn't actually
  exist as a class yet — either implement RAII wrapper around RustBuffer
  or remove the comment
- 3 dead-code warnings in `gen_nitro`: `hybrid_objects` method,
  `constructors` field — annotate or remove
