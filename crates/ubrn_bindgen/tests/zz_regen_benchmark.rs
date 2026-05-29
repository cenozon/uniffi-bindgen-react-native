// THROWAWAY: regenerate a fixture's Nitro C++/TS for fast manual
// compile/repro iteration. Defaults to the benchmark fixture (into its
// generated/ tree); override via env to target any built cdylib:
//   UBRN_REGEN_LIB=uniffi_callbacks UBRN_REGEN_OUT=/tmp/cbgen \
//     cargo test -p ubrn_bindgen --test zz_regen_benchmark -- --nocapture
use camino::Utf8PathBuf;
use std::path::PathBuf;

use ubrn_bindgen::{AbiFlavor, BindingsArgs, OutputArgs, SourceArgs, SwitchArgs};

#[test]
fn regen_benchmark_nitro() {
    let cargo_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = cargo_manifest.parent().unwrap().parent().unwrap();

    let lib_name =
        std::env::var("UBRN_REGEN_LIB").unwrap_or_else(|_| "uniffi_benchmark".to_string());
    // Prefer release for the benchmark (perf), else debug.
    let candidates = [
        root.join(format!("target/release/lib{lib_name}.so")),
        root.join(format!("target/debug/lib{lib_name}.so")),
    ];
    let Some(lib) = candidates.iter().find(|p| p.exists()).cloned() else {
        // CI-safe: skip when the fixture cdylib hasn't been built. This is a
        // manual dev/iteration helper, not a coverage gate.
        eprintln!("skipping: no built cdylib for {lib_name}; build it first");
        return;
    };

    let out = std::env::var("UBRN_REGEN_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("fixtures/benchmark/generated/nitro"));
    let ts_dir = Utf8PathBuf::from_path_buf(out.join("ts")).unwrap();
    let cpp_dir = Utf8PathBuf::from_path_buf(out.join("cpp")).unwrap();
    std::fs::create_dir_all(&ts_dir).unwrap();
    std::fs::create_dir_all(&cpp_dir).unwrap();

    let switches = SwitchArgs {
        flavor: AbiFlavor::Nitro,
    };
    let source = SourceArgs::library(&Utf8PathBuf::from_path_buf(lib).unwrap());
    let output = OutputArgs::new(&ts_dir, &cpp_dir, /* no_format */ true);
    BindingsArgs::new(switches, source, output)
        .run(None)
        .expect("nitro emission");
    eprintln!("regenerated {lib_name} nitro -> {cpp_dir}");
}
