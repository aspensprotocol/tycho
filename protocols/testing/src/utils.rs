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
use tracing::{debug, info};

/// Compile the release WASM binary for the Substreams package in `package_dir`. Does nothing when
/// `prebuilt` holds, since the binary the manifest points at then already exists.
fn build_wasm(package_dir: &str, prebuilt: bool) -> miette::Result<()> {
    if prebuilt {
        info!("Expecting a pre-built WASM binary in {package_dir}");
        return Ok(());
    }

    info!("Building WASM binary in {package_dir}");
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(package_dir)
        // RUSTUP_TOOLCHAIN outranks a rust-toolchain.toml, so the value rustup exported for this
        // crate would build the package with its nightly toolchain, which carries no wasm32
        // target, instead of the version the Substreams workspace pins.
        .env_remove("RUSTUP_TOOLCHAIN")
        // The manifest looks for the binary under the package's own target directory, so a shared
        // one exported by a developer or a CI runner would hide it from `substreams pack`.
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .status()
        .into_diagnostic()
        .wrap_err("Failed to run cargo build for the Substreams package")?;

    if !status.success() {
        return Err(miette!("cargo build failed for the Substreams package in {package_dir}"));
    }

    Ok(())
}

/// Build a Substreams package with modifications to the YAML file. `prebuilt_wasm` skips compiling
/// the package's WASM binary, for callers that already hold one.
pub fn build_spkg(
    yaml_file_path: &PathBuf,
    initial_block: u64,
    prebuilt_wasm: bool,
) -> miette::Result<String> {
    info!("Building spkg from {:?}", yaml_file_path);

    let parent_dir = Path::new(yaml_file_path)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_str()
        .unwrap_or("");

    // `substreams pack` only reads the WASM the manifest points at, so compile it first. Doing
    // this before the backup exists keeps a failure here from leaving the backup file behind.
    build_wasm(parent_dir, prebuilt_wasm)?;

    // Create a backup file of the unmodified Substreams protocol YAML config file.
    let backup_file_path = yaml_file_path.with_extension("backup");
    fs::copy(yaml_file_path, &backup_file_path).into_diagnostic()?;

    let figment = Figment::new().merge(Yaml::file(yaml_file_path));
    let mut data: Value = figment.extract().into_diagnostic()?;

    // Apply the modification function to update the YAML files
    modify_initial_block(&mut data, initial_block);

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

    // Write the modified YAML back to the file
    let yaml_string = serde_yaml::to_string(&data).into_diagnostic()?;
    fs::write(yaml_file_path, yaml_string).into_diagnostic()?;

    // Run the substreams pack command to create the spkg
    if Command::new("substreams")
        .arg("--version")
        .output()
        .is_err()
    {
        return Err(miette!("Substreams CLI is not installed or not found in PATH"));
    }
    let pack_result = Command::new("substreams")
        .arg("pack")
        .arg(yaml_file_path)
        .output();

    // Restore the original YAML from backup before reporting a failed pack, so the manifest does
    // not keep the rewritten initial block.
    fs::copy(&backup_file_path, yaml_file_path).into_diagnostic()?;
    fs::remove_file(&backup_file_path).into_diagnostic()?;

    let output = pack_result
        .into_diagnostic()
        .wrap_err("Failed to run the substreams pack command")?;

    if !output.status.success() {
        return Err(miette!(
            "substreams pack failed for {}: {}",
            yaml_file_path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    debug!("Spkg built successfully: {}", spkg_name);

    Ok(spkg_name)
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

    fn create_test_data() -> Value {
        let file_path = Path::new("src/assets/substreams_example.yaml");
        let figment = Figment::new().merge(Yaml::file(file_path));

        figment
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
}
