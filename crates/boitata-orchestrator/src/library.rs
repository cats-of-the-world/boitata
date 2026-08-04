// Blueprint loading: how `--blueprint <path>` resolves to a [`Graph`].
//
// Blueprints are defined in YAML, never in code, and are entirely user-provided:
// `load` reads a path to a YAML file and compiles it. A set of ready-to-copy
// examples lives under `examples/blueprints/` in the repo (they are not compiled
// into the binary); point `--blueprint` at one of those, or at your own file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

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

/// Discover the blueprints in directory `dir`: a map from each `.yaml`/`.yml`
/// file's stem to its path, sorted by name.
///
/// This is how a host exposes a *fixed, trusted* set of blueprints by name —
/// e.g. `boitata-server --blueprints-dir`, which offers only these files and
/// never resolves an arbitrary path from a network request. Each file is compiled
/// up front so a malformed blueprint fails at startup, not when a user selects it.
pub fn discover(dir: &Path) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let entries = fs::read_dir(dir)
        .with_context(|| format!("failed to read blueprints directory `{}`", dir.display()))?;
    let mut blueprints = BTreeMap::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("failed to read an entry in `{}`", dir.display()))?
            .path();
        let is_yaml = path
            .extension()
            .is_some_and(|ext| ext == "yaml" || ext == "yml");
        if !path.is_file() || !is_yaml {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .with_context(|| format!("blueprint file `{}` has a non-UTF-8 name", path.display()))?
            .to_string();
        let path_str = path
            .to_str()
            .with_context(|| format!("blueprint path `{}` is not valid UTF-8", path.display()))?;
        // Compile now so a broken file surfaces at startup rather than at run time.
        load(path_str)
            .with_context(|| format!("blueprint `{name}` in `{}` is invalid", dir.display()))?;
        blueprints.insert(name, path);
    }
    Ok(blueprints)
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
                let path_str = path
                    .to_str()
                    .unwrap_or_else(|| panic!("non-UTF-8 example path: {}", path.display()));
                load(path_str).unwrap_or_else(|e| {
                    panic!("example `{}` failed to load: {e:#}", path.display())
                });
            }
        }
        assert!(count > 0, "no example blueprints found in {dir}");
    }

    #[test]
    fn discover_maps_names_to_paths_and_validates() {
        // A directory of blueprints is discovered by stem, sorted, and each file
        // is compiled (so a broken one is rejected up front).
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("beta.yaml"),
            "name: b\nentry: a\nnodes:\n  a: {type: tool, tool: cargo_fmt}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("alpha.yml"),
            "name: a\nentry: a\nnodes:\n  a: {type: tool, tool: cargo_fmt}\n",
        )
        .unwrap();
        // A non-YAML file is ignored.
        fs::write(dir.path().join("notes.txt"), "ignore me").unwrap();

        let found = discover(dir.path()).unwrap();
        assert_eq!(
            found.keys().cloned().collect::<Vec<_>>(),
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert!(found["beta"].ends_with("beta.yaml"));

        // A malformed blueprint fails discovery (not silently skipped).
        fs::write(
            dir.path().join("broken.yaml"),
            "name: x\nentry: nope\nnodes: {}\n",
        )
        .unwrap();
        assert!(discover(dir.path()).is_err());
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
