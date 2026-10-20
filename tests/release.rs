use std::{fs, path::Path};

#[test]
fn release_versions_match() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let version = env!("CARGO_PKG_VERSION");
    let package: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("npm/package.json")).unwrap()).unwrap();
    assert_eq!(
        package["version"], version,
        "npm version must match Cargo.toml"
    );
    let formula = fs::read_to_string(root.join("Formula/dev-zero.rb")).unwrap();
    assert!(
        formula.contains(&format!("tag: \"v{version}\"")),
        "Homebrew tag must match Cargo.toml"
    );
    if let Ok(tag) = std::env::var("TAG") {
        assert_eq!(
            tag,
            format!("v{version}"),
            "release tag must match Cargo.toml"
        );
    }
}
