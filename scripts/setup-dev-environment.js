#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const net = require("net");

const PORTS_FILE = path.join(__dirname, "..", ".dev-ports.json");

/**
 * Check if a port is available
 */
function isPortAvailable(port) {
  return new Promise((resolve) => {
    const sock = net.createConnection({ port, host: "localhost" });
    sock.on("connect", () => {
      sock.destroy();
      resolve(false);
    });
    sock.on("error", () => resolve(true));
  });
}

/**
 * Find a free port starting from a given port
 */
async function findFreePort(startPort = 3000) {
  let port = startPort;
  while (!(await isPortAvailable(port))) {
    port++;
    if (port > 65535) {
      throw new Error("No available ports found");
    }
  }
  return port;
}

/**
 * Load existing ports from file
 */
function loadPorts() {
  try {
    if (fs.existsSync(PORTS_FILE)) {
      const data = fs.readFileSync(PORTS_FILE, "utf8");
      return JSON.parse(data);
    }
  } catch (error) {
    console.warn("Failed to load existing ports:", error.message);
  }
  return null;
}

/**
 * Save ports to file
 */
function savePorts(ports) {
  try {
    fs.writeFileSync(PORTS_FILE, JSON.stringify(ports, null, 2));
  } catch (error) {
    console.error("Failed to save ports:", error.message);
    throw error;
  }
}

/**
 * Verify that saved ports are still available
 */
async function verifyPorts(ports) {
  const frontendAvailable = await isPortAvailable(ports.frontend);
  const backendAvailable = await isPortAvailable(ports.backend);
  const mobilePort = ports.mobile || ports.frontend + 2;
  const mobileAvailable = await isPortAvailable(mobilePort);

  if (process.argv[2] === "get" && (!frontendAvailable || !backendAvailable || !mobileAvailable)) {
    console.log(
      `Port availability check failed: frontend:${ports.frontend}=${frontendAvailable}, backend:${ports.backend}=${backendAvailable}, mobile:${mobilePort}=${mobileAvailable}`
    );
  }

  return frontendAvailable && backendAvailable && mobileAvailable;
}

/**
 * Allocate ports for development
 */
async function allocatePorts() {
  // If PORT env is set, use it for frontend, PORT+1 for backend, and PORT+2 for mobile-ui
  if (process.env.PORT) {
    const frontendPort = parseInt(process.env.PORT, 10);
    const backendPort = frontendPort + 1;
    const mobilePort = frontendPort + 2;

    const ports = {
      frontend: frontendPort,
      backend: backendPort,
      mobile: mobilePort,
      timestamp: new Date().toISOString(),
    };

    if (process.argv[2] === "get") {
      console.log("Using PORT environment variable:");
      console.log(`Frontend: ${ports.frontend}`);
      console.log(`Backend: ${ports.backend}`);
      console.log(`Mobile-UI: ${ports.mobile}`);
    }

    return ports;
  }

  // Try to load existing ports first
  const existingPorts = loadPorts();

  if (existingPorts) {
    // Add mobile port if missing
    if (!existingPorts.mobile) {
      existingPorts.mobile = existingPorts.frontend + 2;
      savePorts(existingPorts);
    }

    // Verify existing ports are still available
    if (await verifyPorts(existingPorts)) {
      if (process.argv[2] === "get") {
        console.log("Reusing existing dev ports:");
        console.log(`Frontend: ${existingPorts.frontend}`);
        console.log(`Backend: ${existingPorts.backend}`);
        console.log(`Mobile-UI: ${existingPorts.mobile}`);
      }
      return existingPorts;
    } else {
      if (process.argv[2] === "get") {
        console.log(
          "Existing ports are no longer available, finding new ones..."
        );
      }
    }
  }

  // Find new free ports
  const frontendPort = await findFreePort(3000);
  const backendPort = await findFreePort(frontendPort + 1);
  const mobilePort = await findFreePort(frontendPort + 2);

  const ports = {
    frontend: frontendPort,
    backend: backendPort,
    mobile: mobilePort,
    timestamp: new Date().toISOString(),
  };

  savePorts(ports);

  if (process.argv[2] === "get") {
    console.log("Allocated new dev ports:");
    console.log(`Frontend: ${ports.frontend}`);
    console.log(`Backend: ${ports.backend}`);
    console.log(`Mobile-UI: ${ports.mobile}`);
  }

  return ports;
}

/**
 * Get ports (allocate if needed)
 */
async function getPorts() {
  return await allocatePorts();
}

/**
 * Clear saved ports
 */
function clearPorts() {
  try {
    if (fs.existsSync(PORTS_FILE)) {
      fs.unlinkSync(PORTS_FILE);
      console.log("Cleared saved dev ports");
    } else {
      console.log("No saved ports to clear");
    }
  } catch (error) {
    console.error("Failed to clear ports:", error.message);
  }
}

// CLI interface
if (require.main === module) {
  const command = process.argv[2];

  switch (command) {
    case "get":
      getPorts()
        .then((ports) => {
          console.log(JSON.stringify(ports));
        })
        .catch(console.error);
      break;

    case "clear":
      clearPorts();
      break;

    case "frontend":
      getPorts()
        .then((ports) => {
          console.log(JSON.stringify(ports.frontend, null, 2));
        })
        .catch(console.error);
      break;

    case "backend":
      getPorts()
        .then((ports) => {
          console.log(JSON.stringify(ports.backend, null, 2));
        })
        .catch(console.error);
      break;

    case "mobile-ui":
      getPorts()
        .then((ports) => {
          console.log(JSON.stringify(ports.mobile, null, 2));
        })
        .catch(console.error);
      break;

    default:
      console.log("Usage:");
      console.log(
        "  node setup-dev-environment.js get     - Allocate dev ports"
      );
      console.log(
        "  node setup-dev-environment.js frontend - Get frontend port only"
      );
      console.log(
        "  node setup-dev-environment.js backend  - Get backend port only"
      );
      console.log(
        "  node setup-dev-environment.js mobile-ui - Get mobile-ui port only"
      );
      console.log(
        "  node setup-dev-environment.js clear    - Clear saved ports"
      );
      break;
  }
}

module.exports = { getPorts, clearPorts, findFreePort };
