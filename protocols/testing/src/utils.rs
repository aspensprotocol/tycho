use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use figment::{
    providers::{Format, Yaml},
    value::Value,
    Figment,
};
use miette::{miette, IntoDiagnostic, WrapErr};
use serde::Deserialize;
use tracing::{debug, error, info};

/// Build a Substreams package, returning the path of the packed spkg.
///
/// `initial_block` forces every module to start at that block; pass `None` to pack the manifest
/// as it is, leaving each module's declared `initialBlock` intact.
pub fn build_spkg(yaml_file_path: &PathBuf, initial_block: Option<u64>) -> miette::Result<String> {
    info!("Building spkg from {:?}", yaml_file_path);

    let figment = Figment::new().merge(Yaml::file(yaml_file_path));
    let mut data: Value = figment.extract().into_diagnostic()?;

    let parent_dir = Path::new(yaml_file_path)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_str()
        .unwrap_or("");

    let package_name = data
        .clone()
        .find("package")
        .expect("Package not found on YAML")
        .find("name")
        .expect("Name not found on YAML")
        .as_str()
        .expect("Failed to convert name to string.")
        .replace("_", "-");

    let binding = data
        .clone()
        .find("package")
        .expect("Package not found on YAML")
        .find("version")
        .expect("Version not found on YAML");

    let package_version = binding.as_str().unwrap_or("");
    let spkg_name = format!("{parent_dir}/{package_name}-{package_version}.spkg");

    // Pulling every module to the same block is only correct when the caller asked for a specific
    // start block: modules that legitimately start later would otherwise be forced to start
    // earlier. Back the manifest up first, since packing reads it from disk.
    let backup_file_path = match initial_block {
        Some(initial_block) => {
            let backup_file_path = yaml_file_path.with_extension("backup");
            fs::copy(yaml_file_path, &backup_file_path).into_diagnostic()?;

            modify_initial_block(&mut data, initial_block);
            let yaml_string = serde_yaml::to_string(&data).into_diagnostic()?;
            fs::write(yaml_file_path, yaml_string).into_diagnostic()?;

            Some(backup_file_path)
        }
        None => None,
    };

    // Run the substreams pack command to create the spkg
    if Command::new("substreams")
        .arg("--version")
        .output()
        .is_err()
    {
        return Err(miette!("Substreams CLI is not installed or not found in PATH"));
    }
    match Command::new("substreams")
        .arg("pack")
        .arg(yaml_file_path)
        .output()
    {
        Ok(output) => {
            if !output.status.success() {
                error!(
                    "Substreams pack command failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Err(e) => {
            error!(
                "Error running substreams pack command. Ensure that the wasm target was built. {e:#}",
            );
        }
    }

    // Restore the original YAML from backup
    if let Some(backup_file_path) = backup_file_path {
        fs::copy(&backup_file_path, yaml_file_path).into_diagnostic()?;
        fs::remove_file(&backup_file_path).into_diagnostic()?;
    }
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

/// Update the initial block for all modules in the configuration data.
pub fn modify_initial_block(data: &mut Value, start_block: u64) {
    if let Value::Dict(_, ref mut dict) = data {
        if let Some(Value::Array(_, modules)) = dict.get_mut("modules") {
            for module in modules.iter_mut() {
                if let Value::Dict(_, ref mut module_dict) = module {
                    module_dict.insert("initialBlock".to_string(), Value::from(start_block));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use figment::value::Value;

    use super::*;

    const EXAMPLE_MANIFEST: &str = include_str!("assets/substreams_example.yaml");
    const ANCHORED_MANIFEST: &str = include_str!("assets/substreams_example_anchors.yaml");

    fn create_test_data() -> Value {
        Figment::new()
            .merge(Yaml::string(EXAMPLE_MANIFEST))
            .extract()
            .expect("Failed to parse YAML file")
    }

    #[test]
    fn test_modify_initial_block_normal_case() {
        let mut data = create_test_data();

        // Apply modification
        let new_block = 12345;
        modify_initial_block(&mut data, new_block);

        // Verify all modules now have the correct initialBlock
        if let Value::Dict(_, dict) = &data {
            if let Some(Value::Array(_, modules)) = dict.get("modules") {
                for module in modules {
                    if let Value::Dict(_, module_dict) = module {
                        if let Some(Value::Num(_, block)) = module_dict.get("initialBlock") {
                            assert_eq!(block.to_u128().unwrap(), new_block as u128);
                        } else {
                            panic!("initialBlock not found or has wrong type");
                        }
                    }
                }
            } else {
                panic!("modules not found or has wrong type");
            }
        }
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
