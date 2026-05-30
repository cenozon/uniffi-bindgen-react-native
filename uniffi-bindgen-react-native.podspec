require 'json'
package = JSON.parse(File.read(File.join(__dir__, 'package.json')))

Pod::Spec.new do |s|
  s.name             = package['name']
  s.version          = package['version']
  s.summary          = package['description']
  s.homepage         = package['homepage']
  s.license          = { :type => package['license'], :file => 'LICENSE' }
  s.author           = { package['author']['name'] => package['author']['email'] }
  s.source           = { :git => package['repository']['url'], :tag => s.version.to_s }
  s.platform         = :ios, '13.0'
  # Recursive glob: the single-`*` form omitted the `cpp/includes/nitro-uniffi/`
  # subdir entirely, so the headers the generated code angle-includes
  # (<nitro-uniffi/jsi_converter_ints.hpp>, <nitro-uniffi/js_dispatcher.hpp>, …)
  # were never shipped as pod sources. Mirror how react-native-nitro-modules
  # globs its `cpp/**`.
  s.source_files        = 'cpp/includes/**/*.{h,cpp,hpp}'
  s.public_header_files  = 'cpp/includes/**/*.{h,hpp}'
  # Preserve the cpp/includes/ tree (incl. nitro-uniffi/) when CocoaPods copies
  # headers into Pods/Headers/Public, so both bare angle includes (<RustBuffer.h>)
  # and subdir angle includes (<nitro-uniffi/...>) keep resolving after install.
  s.header_mappings_dir  = 'cpp/includes'
  s.swift_versions       = '4.0'
  s.pod_target_xcconfig = {
    'SWIFT_OPTIMIZATION_LEVEL' => '-Onone',
    # The generated bindings angle-include these headers WITHOUT a pod-name
    # prefix (<RustBuffer.h>, <NitroUniffi.hpp>, <nitro-uniffi/...>), unlike
    # <NitroModules/...> which resolves via pod-name == include-prefix. Put this
    # pod's own include root on the search path so the runtime headers resolve
    # each other's bare includes while this pod compiles.
    'HEADER_SEARCH_PATHS' => '"$(PODS_TARGET_SRCROOT)/cpp/includes"',
  }
  s.dependency 'React-Core'
end
