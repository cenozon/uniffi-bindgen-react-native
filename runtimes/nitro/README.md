# @ubrn/nitro-runtime

The Nitro Modules backend of uniffi-bindgen-react-native (ubrn). Layered over Nitro's HybridObject infrastructure rather than the JSI hostobject middle layer that the TurboModule and `framework: nitro` (shell) paths use.

## What this package ships

- `cpp/nitro-uniffi/*.hpp` — header-only C++ helpers consumed by every ubrn-emitted `Hybrid<Interface>` implementation. Each header narrows in on one piece of the uniffi C ABI surface:
  - `converters.hpp` — primitive + `std::string` lift/lower against `RustBuffer`
  - `status.hpp` — `RustCallStatus` → C++ exception
  - `rust_buffer.hpp` — `RustBuffer` lifetime helper (RAII-owning the Rust-side alloc)
  - `handle.hpp` — opaque object-handle (uint64) lift/lower with Arc semantics
  - `future.hpp` — async machinery: ties uniffi's `rust_future_*` poll cycle to `Promise<T>`
  - `callback.hpp` — JS-implementing-Rust-trait support (vtable construction + lifetime)
  - `record.hpp` — generic record serializer hooks consumed by the per-record emitted code
- `typescript/src/index.ts` — a small TS shim that re-exports the Nitro types ubrn-emitted code expects, plus async/error utilities. Most ubrn-emitted TS imports directly from `react-native-nitro-modules`; this package is a thin compatibility surface.

## How the path differs from `framework: nitro` (shell)

`framework: nitro` emits a single `Hybrid<Name>Installer` HybridObject whose `install()` method bootstraps the legacy globalThis JSI hostobject — JS still talks to the host object, the Nitro layer is just an install hook.

`framework: nitro-native` (this runtime) eliminates the host object entirely. Each uniffi `interface` becomes its own HybridObject, each uniffi method becomes a typed Nitro method, each uniffi record becomes a Nitro struct (auto-converted by Nitrogen's `JSIConverter`), each uniffi callback interface becomes a Nitro function parameter, each `async fn` becomes a `Promise<T>`-returning method. The C-ABI is called directly from the HybridObject method bodies via the helpers in `cpp/nitro-uniffi/`.

## Why a separate npm package

The Rust crate consumers (`runtimes/napi`, `runtimes/core`) live as Rust libraries that ship a Node native addon. The Nitro backend is C++-header-only — it has no Rust code — so a Rust crate would be inert weight. An npm package whose `files` list is just `cpp/` is the right shape; consumers reference these headers from their `CMakeLists.txt` via `require.resolve` (same pattern ubrn's existing CMakeLists already uses for `uniffi-bindgen-react-native/cpp/includes`).
