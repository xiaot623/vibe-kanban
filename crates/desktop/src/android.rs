//! Android-specific functionality for discovering servers on the local network.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use serde::Serialize;
use tokio::net::UdpSocket;

use crate::{DiscoveryBeacon, DISCOVERY_MULTICAST_ADDR, DISCOVERY_PORT, DISCOVERY_SERVICE};

#[derive(Serialize)]
pub struct DiscoveredServer {
    pub ip: String,
    pub port: u16,
    pub hostname: String,
    pub requires_auth: bool,
}

#[tauri::command]
pub async fn discover_servers() -> Result<Vec<DiscoveredServer>, String> {
    let multicast_addr: Ipv4Addr = DISCOVERY_MULTICAST_ADDR
        .parse()
        .map_err(|err: std::net::AddrParseError| err.to_string())?;

    let socket = UdpSocket::bind(format!("0.0.0.0:{}", DISCOVERY_PORT))
        .await
        .map_err(|err| err.to_string())?;

    socket
        .join_multicast_v4(multicast_addr, Ipv4Addr::UNSPECIFIED)
        .map_err(|err| err.to_string())?;

    let mut servers: HashMap<String, DiscoveredServer> = HashMap::new();
    let mut buffer = vec![0u8; 2048];
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(5);

    while start.elapsed() < timeout {
        let remaining = timeout.saturating_sub(start.elapsed());
        let recv = tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await;

        let (len, addr) = match recv {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => return Err(err.to_string()),
            Err(_) => break,
        };

        let beacon: DiscoveryBeacon = match serde_json::from_slice(&buffer[..len]) {
            Ok(beacon) => beacon,
            Err(_) => continue,
        };

        if beacon.service != DISCOVERY_SERVICE || beacon.version != 1 {
            continue;
        }

        let ip = match addr.ip() {
            IpAddr::V4(addr) => addr.to_string(),
            IpAddr::V6(addr) => addr.to_string(),
        };

        servers
            .entry(beacon.fingerprint)
            .or_insert(DiscoveredServer {
                ip,
                port: beacon.port,
                hostname: beacon.hostname,
                requires_auth: beacon.requires_auth,
            });
    }

    Ok(servers.into_values().collect())
}
