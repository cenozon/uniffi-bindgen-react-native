//! One-shot: emit Nitro bindings for ace_db_js into a non-destructive temp dir
//! so the user's existing TURBO output stays put.
//!
//! Run: `cargo run -p ubrn_bindgen --example ace_nitro_emit -- \
//!         <cdylib> <uniffi.toml> <out-cpp-dir> <out-ts-dir>`

use std::env;

use camino::Utf8PathBuf;
use ubrn_bindgen::{AbiFlavor, BindingsArgs, OutputArgs, SourceArgs, SwitchArgs};

fn main() {
    let argv: Vec<String> = env::args().collect();
    if argv.len() != 5 {
        eprintln!(
            "usage: {} <cdylib> <uniffi.toml> <out-cpp-dir> <out-ts-dir>",
            argv[0]
        );
        std::process::exit(2);
    }
    let cdylib = Utf8PathBuf::from(&argv[1]);
    let uniffi_toml = Utf8PathBuf::from(&argv[2]);
    let cpp_dir = Utf8PathBuf::from(&argv[3]);
    let ts_dir = Utf8PathBuf::from(&argv[4]);
    std::fs::create_dir_all(&cpp_dir).expect("mkdir cpp");
    std::fs::create_dir_all(&ts_dir).expect("mkdir ts");

    let args = BindingsArgs::new(
        SwitchArgs {
            flavor: AbiFlavor::Nitro,
        },
        SourceArgs::library(&cdylib).with_config(Some(uniffi_toml)),
        OutputArgs::new(&ts_dir, &cpp_dir, /* no_format */ true),
    );
    args.run(None).expect("nitro emit");
    println!("ok: cpp={cpp_dir} ts={ts_dir}");
}
