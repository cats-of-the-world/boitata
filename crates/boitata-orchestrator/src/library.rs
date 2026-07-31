// The blueprint registry: how `--blueprint <name>` resolves to a [`Graph`].
//
// Blueprints are defined in YAML, never in code. A small starter library ships
// embedded in the binary (below); `load` resolves a value to a starter by name,
// or, failing that, reads it as a path to the user's own YAML file.

use std::fs;
use std::path::Path;

use anyhow::{Context, bail};

use super::Graph;
use super::yaml::from_yaml;

/// The blueprints that ship with boitata: `(name, embedded YAML source)`. The
/// sources live under `blueprints/` and are compiled into the binary so the
/// starters work with no files on disk.
const STARTERS: &[(&str, &str)] = &[
    ("default", include_str!("../blueprints/default.yaml")),
    (
        "fix_lint_errors",
        include_str!("../blueprints/fix_lint_errors.yaml"),
    ),
    (
        "fix_test_failure",
        include_str!("../blueprints/fix_test_failure.yaml"),
    ),
    (
        "setup_devbox",
        include_str!("../blueprints/setup_devbox.yaml"),
    ),
    (
        "human_approval",
        include_str!("../blueprints/human_approval.yaml"),
    ),
    (
        "containerized_task",
        include_str!("../blueprints/containerized_task.yaml"),
    ),
];

/// Names of the built-in starter blueprints, for help and error messages.
pub fn starter_names() -> Vec<&'static str> {
    STARTERS.iter().map(|(name, _)| *name).collect()
}

/// Resolve `name_or_path` to a compiled [`Graph`]: a built-in starter if the
/// name matches one, otherwise a path to a YAML file on disk.
pub fn load(name_or_path: &str) -> anyhow::Result<Graph> {
    if let Some((_, src)) = STARTERS.iter().find(|(name, _)| *name == name_or_path) {
        // A broken embedded starter is a build-time bug, not user error; the
        // `starters_all_load` test guards against it.
        return from_yaml(src)
            .with_context(|| format!("built-in blueprint `{name_or_path}` is invalid"));
    }

    // Only treat the input as a file path when it looks like one — a path
    // separator or a `.yaml`/`.yml` extension. This keeps a mistyped starter
    // name (e.g. `deafult`) from being silently resolved against the filesystem
    // and reported as a missing file rather than an unknown blueprint.
    let path = Path::new(name_or_path);
    let looks_like_path = name_or_path.contains('/')
        // `\` is only a path separator on Windows; on Unix it's a valid filename
        // character, so a bare name containing one shouldn't look like a path.
        || (cfg!(target_os = "windows") && name_or_path.contains('\\'))
        || path
            .extension()
            .is_some_and(|ext| ext == "yaml" || ext == "yml");
    if looks_like_path {
        if !path.is_file() {
            bail!("blueprint file `{name_or_path}` not found");
        }
        let src = fs::read_to_string(path)
            .with_context(|| format!("failed to read blueprint file `{name_or_path}`"))?;
        return from_yaml(&src)
            .with_context(|| format!("failed to load blueprint from `{name_or_path}`"));
    }

    bail!(
        "unknown blueprint `{name_or_path}` (built-ins: {}); pass a path to a .yaml file to use your own",
        starter_names().join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starters_all_load() {
        // Every embedded starter must parse and compile — catches a malformed
        // shipped YAML at test time rather than when a user selects it.
        for name in starter_names() {
            load(name).unwrap_or_else(|e| panic!("starter `{name}` failed to load: {e:#}"));
        }
    }

    #[test]
    fn starters_match_blueprints_dir() {
        // Guard against drift the other way: a `.yaml` added under `blueprints/`
        // but not listed in `STARTERS` (so it would ship unreachable by name).
        // BTreeSet so a mismatch lists entries in a stable, sorted order.
        use std::collections::BTreeSet;
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/blueprints");
        let on_disk: BTreeSet<String> = fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("blueprints dir not found at {dir}: {e}"))
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
            .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
            .collect();
        let registered: BTreeSet<String> = starter_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        assert_eq!(
            on_disk, registered,
            "blueprints/ directory and STARTERS are out of sync"
        );
    }

    #[test]
    fn unknown_name_errors_and_lists_starters() {
        let err = load("does_not_exist").err().unwrap().to_string();
        assert!(err.contains("unknown blueprint"), "{err}");
        assert!(err.contains("default"), "{err}");
    }

    #[test]
    fn missing_path_reports_file_not_found() {
        // A path-shaped argument that doesn't exist is a missing file, not an
        // "unknown blueprint" (which is reserved for bare-name typos).
        let err = load("./nope.yaml").err().unwrap().to_string();
        assert!(err.contains("not found"), "{err}");
        assert!(!err.contains("unknown blueprint"), "{err}");
    }

    #[test]
    fn loads_from_a_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.yaml");
        fs::write(
            &path,
            "name: custom\nentry: a\nnodes:\n  a: {type: tool, tool: cargo_fmt}\n",
        )
        .unwrap();
        assert!(load(path.to_str().unwrap()).is_ok());
    }
}
