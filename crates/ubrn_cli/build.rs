use std::{env, fs, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../uniffi-runtime-javascript/Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let contents = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    let version = parse_package_version(&contents)
        .unwrap_or_else(|| panic!("could not find [package] version in {}", manifest.display()));
    println!("cargo:rustc-env=UBRN_RUNTIME_JAVASCRIPT_VERSION={version}");
}

fn parse_package_version(toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('[') {
            in_package = rest.trim_end().trim_end_matches(']') == "package";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("version") else {
            continue;
        };
        let rest = rest.trim_start().strip_prefix('=')?.trim();
        let rest = rest.strip_prefix('"')?;
        let end = rest.find('"')?;
        return Some(rest[..end].to_string());
    }
    None
}
