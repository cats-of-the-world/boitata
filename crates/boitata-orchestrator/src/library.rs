// Blueprint loading: how `--blueprint <path>` resolves to a [`Graph`].
//
// Blueprints are defined in YAML, never in code, and are entirely user-provided:
// `load` reads a path to a YAML file and compiles it. A set of ready-to-copy
// examples lives under `examples/blueprints/` in the repo (they are not compiled
// into the binary); point `--blueprint` at one of those, or at your own file.

use std::fs;
use std::path::Path;

use anyhow::{Context, bail};

use super::Graph;
use super::yaml::from_yaml;

/// Resolve `path` to a compiled [`Graph`] by reading it as a YAML file on disk.
/// Blueprints are always user-provided files; see `examples/blueprints/` for
/// ready-to-copy starting points.
pub fn load(path: &str) -> anyhow::Result<Graph> {
    let file = Path::new(path);
    if !file.is_file() {
        bail!(
            "blueprint file `{path}` not found (blueprints are YAML files; \
             see examples/blueprints/ for ready-to-copy starting points)"
        );
    }
    let src = fs::read_to_string(file)
        .with_context(|| format!("failed to read blueprint file `{path}`"))?;
    from_yaml(&src).with_context(|| format!("failed to load blueprint from `{path}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shipped example blueprint must parse and compile — catches a
    /// malformed example at test time rather than when a user copies it.
    #[test]
    fn example_blueprints_all_load() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/blueprints");
        let entries = fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("examples/blueprints not found at {dir}: {e}"));
        let mut count = 0;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "yaml") {
                count += 1;
                load(path.to_str().unwrap()).unwrap_or_else(|e| {
                    panic!("example `{}` failed to load: {e:#}", path.display())
                });
            }
        }
        assert!(count > 0, "no example blueprints found in {dir}");
    }

    #[test]
    fn missing_path_reports_file_not_found() {
        let err = load("./nope.yaml").err().unwrap().to_string();
        assert!(err.contains("not found"), "{err}");
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
