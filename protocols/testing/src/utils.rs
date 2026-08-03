use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use miette::{ensure, miette, IntoDiagnostic, WrapErr};
use serde::Deserialize;
use tracing::{debug, info};

/// Build a Substreams package, returning the path of the packed spkg.
///
/// `initial_block` forces every module to start at that block; pass `None` to pack the manifest
/// as it is, leaving each module's declared `initialBlock` intact.
pub fn build_spkg(yaml_file_path: &PathBuf, initial_block: Option<u64>) -> miette::Result<String> {
    info!("Building spkg from {:?}", yaml_file_path);

    let content = fs::read_to_string(yaml_file_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to read {}", yaml_file_path.display()))?;
    let mut data: serde_yaml::Value = serde_yaml::from_str(&content)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to parse {}", yaml_file_path.display()))?;

    let parent_dir = yaml_file_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    // Mirrors the `{spkgDefaultName}` the CLI would pick, so that packing to an explicit path
    // still yields the conventional file name.
    let package_field = |field: &str| -> miette::Result<&str> {
        data.get("package")
            .and_then(|package| package.get(field))
            .and_then(|value| value.as_str())
            .ok_or_else(|| miette!("`package.{field}` not found in {}", yaml_file_path.display()))
    };
    let spkg_file_name =
        format!("{}-{}.spkg", package_field("name")?.replace('_', "-"), package_field("version")?);
    let spkg_name = parent_dir
        .join(&spkg_file_name)
        .to_string_lossy()
        .to_string();

    ensure!(
        Command::new("substreams")
            .arg("--version")
            .output()
            .is_ok(),
        "Substreams CLI is not installed or not found in PATH"
    );

    // The manifest is piped in as `-` so the checked-in file is never rewritten. Its relative
    // paths (the wasm binary, the proto import paths) resolve against the working directory rather
    // than the manifest, so pack from the directory holding it.
    let mut child = Command::new("substreams")
        .args(["pack", "-", "--output-file", &spkg_file_name])
        .current_dir(parent_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .into_diagnostic()
        .wrap_err("Failed to spawn the substreams pack command")?;

    let piped = {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| miette!("Failed to open the stdin of the substreams pack command"))?;

        // Pulling every module to the same block is only correct when the caller asked for a
        // specific start block: modules that legitimately start later would otherwise be
        // forced to start earlier. Without an override the manifest goes over the pipe
        // verbatim.
        //
        // A rejected manifest makes pack exit before the write finishes, and its stderr is the
        // useful diagnostic, so a broken pipe here is reported only if pack itself
        // succeeded.
        match initial_block {
            Some(initial_block) => {
                modify_initial_block(&mut data, initial_block);
                serde_yaml::to_writer(&mut stdin, &data).into_diagnostic()
            }
            None => stdin
                .write_all(content.as_bytes())
                .into_diagnostic(),
        }
    };

    let output = child
        .wait_with_output()
        .into_diagnostic()?;

    ensure!(
        output.status.success(),
        "Substreams pack command failed. Ensure that the wasm target was built.\n{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    piped.wrap_err("Failed to pipe the manifest into the substreams pack command")?;

    debug!("Spkg built successfully: {}", spkg_name);

    Ok(spkg_name)
}

/// Extract the lowest `initialBlock` declared by any module of a Substreams manifest.
///
/// The manifest is parsed as YAML, so anchors and aliases are resolved: both
/// `initialBlock: &initial_block 123` and `initialBlock: *initial_block` yield `123`.
/// Fails if the manifest cannot be parsed or if no module declares an `initialBlock`.
pub fn extract_initial_block(yaml: &str) -> miette::Result<u64> {
    #[derive(Deserialize)]
    struct Manifest {
        modules: Vec<Module>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Module {
        initial_block: Option<u64>,
    }

    let manifest: Manifest = serde_yaml::from_str(yaml)
        .into_diagnostic()
        .wrap_err("Failed to parse Substreams manifest")?;

    manifest
        .modules
        .iter()
        .filter_map(|module| module.initial_block)
        .min()
        .ok_or_else(|| {
            miette!(
                "No module declares an `initialBlock`. Please specify it explicitly with \
                 --initial-block."
            )
        })
}

/// Update the initial block of every module that declares one in a parsed Substreams manifest.
///
/// Modules leaving `initialBlock` implicit keep it that way: Substreams derives theirs from their
/// inputs, which land on `start_block` anyway.
pub fn modify_initial_block(manifest: &mut serde_yaml::Value, start_block: u64) {
    let Some(modules) = manifest
        .get_mut("modules")
        .and_then(|modules| modules.as_sequence_mut())
    else {
        return;
    };

    for module in modules {
        if let Some(initial_block) = module.get_mut("initialBlock") {
            *initial_block = start_block.into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_MANIFEST: &str = include_str!("assets/substreams_example.yaml");
    const ANCHORED_MANIFEST: &str = include_str!("assets/substreams_example_anchors.yaml");

    #[test]
    fn test_modify_initial_block_normal_case() {
        let mut manifest: serde_yaml::Value =
            serde_yaml::from_str(EXAMPLE_MANIFEST).expect("Failed to parse YAML");
        let new_block = 12345;

        modify_initial_block(&mut manifest, new_block);

        let modules = manifest["modules"]
            .as_sequence()
            .expect("modules not found or has wrong type");
        assert!(!modules.is_empty());
        for module in modules {
            assert_eq!(module["initialBlock"].as_u64(), Some(new_block));
        }
    }

    #[test]
    fn test_modify_initial_block_leaves_implicit_modules_alone() {
        let mut manifest: serde_yaml::Value = serde_yaml::from_str(
            r"
modules:
  - name: map_a
    initialBlock: 100
  - name: store_b
",
        )
        .expect("Failed to parse YAML");

        modify_initial_block(&mut manifest, 12345);

        let modules = manifest["modules"]
            .as_sequence()
            .expect("modules not found or has wrong type");
        assert_eq!(modules[0]["initialBlock"].as_u64(), Some(12345));
        assert!(
            modules[1].get("initialBlock").is_none(),
            "store_b gained an initialBlock it did not declare"
        );
    }

    #[test]
    fn test_extract_initial_block() {
        let block =
            extract_initial_block(EXAMPLE_MANIFEST).expect("Failed to extract initialBlock");

        assert_eq!(block, 1000000);
    }

    #[test]
    fn test_extract_initial_block_returns_lowest_across_modules() {
        let manifest = r"
modules:
  - name: map_a
    initialBlock: 3000000
  - name: store_b
  - name: map_c
    initialBlock: 1500000
  - name: map_d
    initialBlock: 2000000
";

        let block = extract_initial_block(manifest).expect("Failed to extract initialBlock");

        assert_eq!(block, 1500000);
    }

    #[test]
    fn test_extract_initial_block_with_anchors() {
        let block =
            extract_initial_block(ANCHORED_MANIFEST).expect("Failed to extract initialBlock");

        assert_eq!(block, 1000000);
    }

    // The anchor sits under `defaults` rather than on a module so that the alias is the only path
    // to 1000000. Anchoring on a module's own `initialBlock`, as real manifests do, would let the
    // anchor site supply that value directly and the test would pass even if aliases were skipped.
    #[test]
    fn test_extract_initial_block_resolves_aliases() {
        let manifest = r"
defaults:
  initialBlock: &initial_block 1000000
modules:
  - name: map_a
    initialBlock: 2000000
  - name: map_b
    initialBlock: *initial_block
";

        let block = extract_initial_block(manifest).expect("Failed to extract initialBlock");

        assert_eq!(block, 1000000);
    }

    #[test]
    fn test_extract_initial_block_missing() {
        let manifest = r"
modules:
  - name: map_protocol_changes
    kind: map
";

        let err = extract_initial_block(manifest).expect_err("Expected missing initialBlock error");

        assert!(err
            .to_string()
            .contains("declares an `initialBlock`"));
    }
}
