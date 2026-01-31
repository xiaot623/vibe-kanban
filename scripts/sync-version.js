#!/usr/bin/env node

const fs = require('fs');
const path = require('path');

const rootDir = path.join(__dirname, '..');

// File paths relative to root
const FILES = {
  rootPackageJson: path.join(rootDir, 'package.json'),
  frontendPackageJson: path.join(rootDir, 'frontend', 'package.json'),
  cargoToml: path.join(rootDir, 'Cargo.toml'),
  tauriConf: path.join(rootDir, 'crates', 'desktop', 'tauri.conf.json'),
};

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf-8'));
}

function writeJson(filePath, data) {
  fs.writeFileSync(filePath, JSON.stringify(data, null, 2) + '\n');
}

function readToml(filePath) {
  return fs.readFileSync(filePath, 'utf-8');
}

function writeToml(filePath, content) {
  fs.writeFileSync(filePath, content);
}

function getCurrentVersion() {
  const pkg = readJson(FILES.rootPackageJson);
  return pkg.version;
}

function bumpVersion(version, type) {
  const parts = version.split('.').map(Number);
  switch (type) {
    case 'major':
      return `${parts[0] + 1}.0.0`;
    case 'minor':
      return `${parts[0]}.${parts[1] + 1}.0`;
    case 'patch':
      return `${parts[0]}.${parts[1]}.${parts[2] + 1}`;
    default:
      throw new Error(`Unknown version type: ${type}`);
  }
}

function updatePackageJson(filePath, newVersion) {
  const pkg = readJson(filePath);
  const oldVersion = pkg.version;
  pkg.version = newVersion;
  writeJson(filePath, pkg);
  return oldVersion;
}

function updateCargoToml(filePath, newVersion) {
  let content = readToml(filePath);

  // Update version in [workspace.package] section
  const workspacePackageRegex = /(\[workspace\.package\][\s\S]*?version\s*=\s*")[^"]+(")/;
  if (workspacePackageRegex.test(content)) {
    content = content.replace(workspacePackageRegex, `$1${newVersion}$2`);
    writeToml(filePath, content);
    return true;
  }
  return false;
}

function updateTauriConf(filePath, newVersion) {
  if (!fs.existsSync(filePath)) {
    return false;
  }
  const config = readJson(filePath);
  config.version = newVersion;
  writeJson(filePath, config);
  return true;
}

function showCurrentVersions() {
  console.log('\nCurrent versions:');
  console.log(`  Root package.json:     ${readJson(FILES.rootPackageJson).version}`);
  console.log(`  Frontend package.json: ${readJson(FILES.frontendPackageJson).version}`);

  const cargoContent = readToml(FILES.cargoToml);
  const match = cargoContent.match(/\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"/);
  if (match) {
    console.log(`  Cargo.toml (workspace): ${match[1]}`);
  }

  if (fs.existsSync(FILES.tauriConf)) {
    console.log(`  tauri.conf.json:       ${readJson(FILES.tauriConf).version}`);
  }
}

function syncAllVersions(newVersion) {
  console.log(`\nSyncing all versions to: ${newVersion}`);

  // Update root package.json
  const oldRoot = updatePackageJson(FILES.rootPackageJson, newVersion);
  console.log(`  ✓ Root package.json: ${oldRoot} → ${newVersion}`);

  // Update frontend package.json
  const oldFrontend = updatePackageJson(FILES.frontendPackageJson, newVersion);
  console.log(`  ✓ Frontend package.json: ${oldFrontend} → ${newVersion}`);

  // Update Cargo.toml workspace version
  if (updateCargoToml(FILES.cargoToml, newVersion)) {
    console.log(`  ✓ Cargo.toml [workspace.package]: → ${newVersion}`);
  }

  // Update tauri.conf.json
  if (updateTauriConf(FILES.tauriConf, newVersion)) {
    console.log(`  ✓ tauri.conf.json: → ${newVersion}`);
  }

  console.log('\nVersion sync complete!');
}

function printUsage() {
  console.log(`
Usage: node scripts/sync-version.js [command] [options]

Commands:
  show                    Show current versions across all files
  set <version>           Set a specific version (e.g., 1.2.3)
  bump <patch|minor|major> Bump version by type

Examples:
  node scripts/sync-version.js show
  node scripts/sync-version.js set 1.0.0
  node scripts/sync-version.js bump patch
  node scripts/sync-version.js bump minor
  node scripts/sync-version.js bump major

Or use npm scripts:
  pnpm run version:show
  pnpm run version:set 1.0.0
  pnpm run version:bump patch
`);
}

// Main
const args = process.argv.slice(2);
const command = args[0];

if (!command || command === 'show') {
  showCurrentVersions();
} else if (command === 'set') {
  const version = args[1];
  if (!version) {
    console.error('Error: Please provide a version number');
    console.error('Example: node scripts/sync-version.js set 1.0.0');
    process.exit(1);
  }
  if (!/^\d+\.\d+\.\d+$/.test(version)) {
    console.error('Error: Invalid version format. Use semver format (e.g., 1.2.3)');
    process.exit(1);
  }
  syncAllVersions(version);
} else if (command === 'bump') {
  const type = args[1];
  if (!['patch', 'minor', 'major'].includes(type)) {
    console.error('Error: Please specify bump type: patch, minor, or major');
    console.error('Example: node scripts/sync-version.js bump patch');
    process.exit(1);
  }
  const currentVersion = getCurrentVersion();
  const newVersion = bumpVersion(currentVersion, type);
  console.log(`Bumping ${type}: ${currentVersion} → ${newVersion}`);
  syncAllVersions(newVersion);
} else if (command === 'help' || command === '-h' || command === '--help') {
  printUsage();
} else {
  console.error(`Unknown command: ${command}`);
  printUsage();
  process.exit(1);
}
