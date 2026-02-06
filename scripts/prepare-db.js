#!/usr/bin/env node

const { spawn } = require('child_process');
const fs = require('fs');
const path = require('path');

const checkMode = process.argv.includes('--check');
const debugMode = process.argv.includes('--debug') || process.env.DEBUG_SQLX;

function log(...args) {
  const timestamp = new Date().toISOString();
  console.log(`[${timestamp}]`, ...args);
}

function runCommand(command, args, env) {
  return new Promise((resolve, reject) => {
    log(`Starting: ${command} ${args.join(' ')}`);
    log(`Working directory: ${process.cwd()}`);
    log(`DATABASE_URL: ${env.DATABASE_URL}`);

    const child = spawn(command, args, {
      env: { ...process.env, ...env },
      stdio: 'inherit', // 始终使用 inherit 以便看到所有输出
    });

    // 10分钟超时
    const timeout = setTimeout(() => {
      log('ERROR: Command timed out after 10 minutes!');
      child.kill('SIGTERM');
      setTimeout(() => child.kill('SIGKILL'), 5000);
    }, 10 * 60 * 1000);

    child.on('close', (code) => {
      clearTimeout(timeout);
      log(`Command exited with code: ${code}`);

      if (code === 0) {
        resolve();
      } else {
        reject(new Error(`Command failed with code ${code}`));
      }
    });

    child.on('error', (err) => {
      clearTimeout(timeout);
      log('Command error:', err.message);
      reject(err);
    });
  });
}

async function main() {
  log(checkMode ? 'Checking SQLx prepared queries...' : 'Preparing database for SQLx...');

  // Change to backend directory
  const backendDir = path.join(__dirname, '..', 'crates/db');
  process.chdir(backendDir);

  // Create temporary database file
  const dbFile = path.join(backendDir, 'prepare_db.sqlite');
  fs.writeFileSync(dbFile, '');

  try {
    // Get absolute path (cross-platform)
    const dbPath = path.resolve(dbFile);
    const databaseUrl = `sqlite:${dbPath}`;

    log(`Using database: ${databaseUrl}`);

    // Run migrations
    log('Running migrations...');
    await runCommand('cargo', ['sqlx', 'migrate', 'run'], { DATABASE_URL: databaseUrl });

    // Prepare queries
    log('Preparing queries (this may take several minutes)...');
    const sqlxArgs = checkMode ? ['sqlx', 'prepare', '--check'] : ['sqlx', 'prepare'];
    await runCommand('cargo', sqlxArgs, { DATABASE_URL: databaseUrl });

    log(checkMode ? 'SQLx check complete!' : 'Database preparation complete!');

  } finally {
    // Clean up temporary file
    if (fs.existsSync(dbFile)) {
      log('Cleaning up temporary database file...');
      fs.unlinkSync(dbFile);
    }
  }
}

main().catch(err => {
  log('Error:', err.message);
  process.exit(1);
});
