use std::{env, path::PathBuf, sync::OnceLock};

use tokio::fs;

/// In-process shared port for same-process consumers (e.g. telegram bot).
static SHARED_PORT: OnceLock<u16> = OnceLock::new();

/// Store the server port in-process so other components can read it without
/// relying on the port file or environment variables.
pub fn set_shared_port(port: u16) {
    let _ = SHARED_PORT.set(port);
}

/// Read the port previously stored via [`set_shared_port`].
pub fn get_shared_port() -> Option<u16> {
    SHARED_PORT.get().copied()
}

pub async fn write_port_file(port: u16) -> std::io::Result<PathBuf> {
    let dir = env::temp_dir().join("vibe-kanban");
    let path = dir.join("vibe-kanban.port");
    tracing::debug!("Writing port {} to {:?}", port, path);
    fs::create_dir_all(&dir).await?;
    fs::write(&path, port.to_string()).await?;
    Ok(path)
}

pub async fn read_port_file(app_name: &str) -> std::io::Result<u16> {
    let dir = env::temp_dir().join(app_name);
    let path = dir.join(format!("{app_name}.port"));
    tracing::debug!("Reading port from {:?}", path);

    let content = fs::read_to_string(&path).await?;
    let port: u16 = content
        .trim()
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    Ok(port)
}
