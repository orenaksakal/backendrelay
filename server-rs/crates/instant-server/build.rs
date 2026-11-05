use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=migrations");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    embed_build_version(&manifest);
    let migrations_dir = manifest.join("migrations");
    let mut migrations = BTreeMap::<u64, (Option<(String, PathBuf)>, Option<PathBuf>)>::new();
    for entry in fs::read_dir(&migrations_dir).expect("read embedded migrations") {
        let path = entry.expect("read migration entry").path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((prefix, direction)) = file_name.rsplit_once('.') else {
            continue;
        };
        if direction != "sql" {
            continue;
        }
        let Some((stem, direction)) = prefix.rsplit_once('.') else {
            continue;
        };
        let Some((version, name)) = stem.split_once('_') else {
            panic!("migration file has no numeric prefix: {file_name}");
        };
        let version = version
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("migration has invalid version: {file_name}"));
        let value = migrations.entry(version).or_insert_with(|| (None, None));
        match direction {
            "up" => {
                assert!(value.0.is_none(), "duplicate up migration {version}");
                value.0 = Some((name.to_owned(), path));
            }
            "down" => {
                assert!(value.1.is_none(), "duplicate down migration {version}");
                value.1 = Some(path);
            }
            _ => panic!("migration has invalid direction: {file_name}"),
        }
    }
    assert!(!migrations.is_empty(), "no migrations found");
    let mut generated = String::from("pub(crate) const MIGRATIONS: &[EmbeddedMigration] = &[\n");
    for (index, (version, (up, down))) in migrations.into_iter().enumerate() {
        let expected = u64::try_from(index).unwrap() + 1;
        assert_eq!(version, expected, "migration versions must be contiguous");
        let (name, up) = up.unwrap_or_else(|| panic!("migration {version} has no up file"));
        let down = down.unwrap_or_else(|| panic!("migration {version} has no down file"));
        let additive = fs::read_to_string(&up)
            .unwrap_or_else(|error| panic!("read migration {}: {error}", up.display()))
            .lines()
            .next()
            .is_some_and(|line| line.trim_end() == "-- compat: additive");
        writeln!(
            generated,
            "EmbeddedMigration {{ version: {version}, name: {name:?}, additive: {additive}, up: include_str!(r#\"{}\"#), down: include_str!(r#\"{}\"#) }},",
            up.display(),
            down.display(),
        )
        .unwrap();
    }
    generated.push_str("];\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("migrations.rs");
    fs::write(output, generated).expect("write embedded migration manifest");
}

/// Embeds the release version and git revision used by `--version`, startup
/// logging and `GET /health/version`.
fn embed_build_version(manifest: &Path) {
    println!("cargo:rerun-if-env-changed=BACKENDRELAY_VERSION");
    println!("cargo:rerun-if-env-changed=BACKENDRELAY_GIT_SHA");

    let package_version = env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION");
    let version =
        non_empty_env("BACKENDRELAY_VERSION").unwrap_or_else(|| format!("{package_version}-dev"));
    println!("cargo:rustc-env=BACKENDRELAY_VERSION={version}");

    if let Some(sha) = non_empty_env("BACKENDRELAY_GIT_SHA") {
        println!("cargo:rustc-env=BACKENDRELAY_GIT_SHA={sha}");
        return;
    }
    let sha = git_output(manifest, &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=BACKENDRELAY_GIT_SHA={sha}");
    watch_git_head(manifest);
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn git_output(manifest: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(manifest)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

/// Rebuilds when the checked-out commit changes so the embedded SHA stays
/// accurate. `HEAD` may be a symbolic ref, so watch its target as well.
fn watch_git_head(manifest: &Path) {
    if let Some(head) = git_output(
        manifest,
        &["rev-parse", "--path-format=absolute", "--git-path", "HEAD"],
    ) {
        let head = PathBuf::from(head);
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display());
        }
        if let Ok(contents) = fs::read_to_string(&head)
            && let Some(reference) = contents.trim().strip_prefix("ref:")
            && let Some(reference) = git_output(
                manifest,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    reference.trim(),
                ],
            )
        {
            let reference = PathBuf::from(reference);
            if reference.exists() {
                println!("cargo:rerun-if-changed={}", reference.display());
            }
        }
    }
}
