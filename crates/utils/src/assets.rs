use rust_embed::RustEmbed;

const PROJECT_ROOT: &str = env!("CARGO_MANIFEST_DIR");

pub fn asset_dir() -> std::path::PathBuf {
    let path = if cfg!(debug_assertions) {
        std::path::PathBuf::from(PROJECT_ROOT).join("../../dev_assets")
    } else {
        dirs::home_dir()
            .map(|dir| dir.join(".kanban"))
            .expect("OS didn't give us a home directory")
    };

    // Ensure the directory exists
    if !path.exists() {
        std::fs::create_dir_all(&path).expect("Failed to create asset directory");
    }

    path
    // Path: ~/.kanban
}

pub fn config_path() -> std::path::PathBuf {
    asset_dir().join("config.json")
}

pub fn profiles_path() -> std::path::PathBuf {
    asset_dir().join("profiles.json")
}

pub fn credentials_path() -> std::path::PathBuf {
    asset_dir().join("credentials.json")
}

#[derive(RustEmbed)]
#[folder = "../../assets/sounds"]
pub struct SoundAssets;

#[derive(RustEmbed)]
#[folder = "../../assets/scripts"]
pub struct ScriptAssets;

#[derive(RustEmbed)]
#[folder = "../../assets/config"]
pub struct ConfigAssets;

pub fn default_config() -> Vec<u8> {
    ConfigAssets::get("default_config.json")
        .expect("default_config.json not found in embedded assets")
        .data
        .into_owned()
}

pub fn default_profiles() -> Vec<u8> {
    ConfigAssets::get("default_profiles.json")
        .expect("default_profiles.json not found in embedded assets")
        .data
        .into_owned()
}

pub fn default_mcp() -> Vec<u8> {
    ConfigAssets::get("default_mcp.json")
        .expect("default_mcp.json not found in embedded assets")
        .data
        .into_owned()
}
