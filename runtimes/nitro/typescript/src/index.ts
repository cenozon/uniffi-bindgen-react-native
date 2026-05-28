// SPDX-License-Identifier: MPL-2.0
//
// The Nitro-native runtime's TS shim. Most ubrn-emitted bindings import
// types directly from `react-native-nitro-modules`; this module exists to
// (a) re-export the subset they actually use under a name they can rely
// on across nitro-modules versions, and (b) host small adapters that would
// otherwise be duplicated in every generated file.
//
// As the codegen surfaces grow (callbacks, errors, etc.), this is where
// the corresponding helpers will land — the codegen emits `import { … }
// from '@ubrn/nitro-runtime'` and stays version-stable even if the
// underlying nitro-modules export shape shifts.

export type { HybridObject } from 'react-native-nitro-modules'
export { NitroModules } from 'react-native-nitro-modules'

/**
 * Wraps a synchronous block that interacts with the uniffi C ABI. The
 * codegen wraps every method body in this so a thrown C++ exception (which
 * Nitro lifts to a JS `Error` via `HybridFunction::callMethod`) is
 * re-thrown verbatim with a useful module-qualified message.
 *
 * In its current form this is a thin pass-through — once the codegen
 * starts encoding typed-error variants we'll grow the implementation to
 * decode + re-throw the project-specific subtype here.
 */
export function uniffiCall<T>(fn: () => T): T {
  return fn()
}
