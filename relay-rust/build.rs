use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"));
    let pin_path = manifest_dir.join("../version.pin");
    println!("cargo:rerun-if-changed=../version.pin");

    let contents = fs::read_to_string(&pin_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", pin_path.display()));

    let mut version = None;
    let mut version_code = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("VERSION=") {
            version = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("ANDROID_VERSION_CODE=") {
            version_code = Some(value.trim().to_owned());
        }
    }

    let version = version.expect("VERSION is missing from version.pin");
    let version_code = version_code.expect("ANDROID_VERSION_CODE is missing from version.pin");
    let cargo_version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is not set");

    if version != cargo_version {
        panic!(
            "version.pin VERSION ({version}) does not match Cargo.toml version ({cargo_version})"
        );
    }
    if version_code.is_empty() || !version_code.chars().all(|c| c.is_ascii_digit()) {
        panic!("ANDROID_VERSION_CODE must be a non-empty integer");
    }

    println!("cargo:rustc-env=GNIREHTET_ANDROID_VERSION_CODE={version_code}");
}
