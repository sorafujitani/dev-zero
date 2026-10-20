use std::{
    collections::HashSet,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;
use tempfile::TempDir;

use super::{BINARY, checked, run};

const TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
];

fn fixture() -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in [
        "README.md",
        "LICENSE",
        "scripts/package-npm.sh",
        "npm/package.json",
        "npm/bin/dz.cjs",
    ] {
        let destination = directory.path().join(name);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(source.join(name), destination).unwrap();
    }
    fs::create_dir(directory.path().join("dist")).unwrap();
    fs::copy(BINARY, directory.path().join("dz")).unwrap();
    directory
}

fn archive(root: &Path, target: &str) -> PathBuf {
    root.join(format!(
        "dist/dev-zero-v{}-{target}.tar.gz",
        env!("CARGO_PKG_VERSION")
    ))
}

fn assemble(root: &Path) -> Command {
    let mut command = Command::new("sh");
    command
        .arg(root.join("scripts/package-npm.sh"))
        .arg(root.join("dist"))
        .env("npm_config_cache", root.join("cache"))
        .current_dir(root);
    command
}

fn write_archives(root: &Path) {
    // Only the native binary is executed; other targets are assembly fixtures.
    for target in TARGETS {
        checked(
            Command::new("tar")
                .args(["-czf"])
                .arg(archive(root, target))
                .arg("-C")
                .arg(root)
                .arg("dz"),
        );
    }
}

#[test]
fn npm_assembly_and_offline_installation() {
    let directory = fixture();
    let root = directory.path();
    write_archives(root);
    checked(&mut assemble(root));
    let packed: Value = serde_json::from_str(&checked(
        Command::new("npm")
            .args(["pack", "./npm", "--json", "--pack-destination", "dist"])
            .env("npm_config_cache", root.join("cache"))
            .current_dir(root),
    ))
    .unwrap();
    let paths: HashSet<_> = packed[0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    for platform in ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"] {
        assert!(paths.contains(format!("prebuilt/{platform}/dz").as_str()));
    }
    for name in ["README.md", "LICENSE", "bin/dz.cjs"] {
        assert!(paths.contains(name));
    }
    assert!(!paths.iter().any(|path| path.starts_with("test/")));
    checked(
        Command::new("npm")
            .args([
                "install",
                "--offline",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--prefix",
                "installed",
            ])
            .arg(
                root.join("dist")
                    .join(packed[0]["filename"].as_str().unwrap()),
            )
            .env("npm_config_cache", root.join("cache"))
            .current_dir(root),
    );
    let launcher = root.join("installed/node_modules/.bin/dz");
    assert_ne!(
        fs::metadata(&launcher).unwrap().permissions().mode() & 0o111,
        0
    );
    assert_eq!(
        checked(Command::new(&launcher).arg("--version")).trim(),
        format!("dz {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(checked(Command::new(&launcher).arg("--help")).contains("dev-null.sh"));
    let failed = run(Command::new(launcher).arg("unknown-command"));
    assert_eq!(failed.status.code(), Some(2));
    assert!(failed.stdout.is_empty());
}

#[test]
fn npm_assembly_rejects_missing_and_nonregular_binaries() {
    let directory = fixture();
    let root = directory.path();
    write_archives(root);
    fs::remove_file(archive(root, TARGETS[3])).unwrap();
    assert!(!run(&mut assemble(root)).status.success());
    assert!(!root.join("npm/prebuilt").exists());

    fs::remove_file(root.join("dz")).unwrap();
    symlink("outside", root.join("dz")).unwrap();
    write_archives(root);
    assert!(!run(&mut assemble(root)).status.success());
    assert!(!root.join("npm/prebuilt").exists());

    fs::remove_file(root.join("dz")).unwrap();
    fs::copy(BINARY, root.join("dz")).unwrap();
    write_archives(root);
    checked(
        Command::new("tar")
            .arg("-czf")
            .arg(archive(root, TARGETS[0]))
            .arg("-C")
            .arg(root)
            .args(["dz", "dz"]),
    );
    assert!(!run(&mut assemble(root)).status.success());
    assert!(!root.join("npm/prebuilt").exists());
}
