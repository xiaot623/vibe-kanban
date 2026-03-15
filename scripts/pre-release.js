#!/usr/bin/env node

const { spawnSync } = require('child_process');
const path = require('path');

const tag = process.argv[2];

if (!tag || tag === '-h' || tag === '--help') {
  console.log('Usage: pnpm run pre_release -- v1.2.3-rc.1');
  process.exit(tag ? 0 : 1);
}

if (!/^v\d+\.\d+\.\d+-rc\.\d+$/.test(tag)) {
  console.error(`Invalid pre-release tag: ${tag}`);
  console.error('Expected format: v1.2.3-rc.1');
  process.exit(1);
}

function run(cmd, args) {
  const result = spawnSync(cmd, args, {
    stdio: 'inherit',
    cwd: path.join(__dirname, '..'),
  });
  if (result.status !== 0) {
    process.exit(result.status || 1);
  }
}

function readOutput(cmd, args) {
  const result = spawnSync(cmd, args, {
    encoding: 'utf-8',
    cwd: path.join(__dirname, '..'),
  });
  if (result.status !== 0) {
    process.exit(result.status || 1);
  }
  return result.stdout.trim();
}

const existingTag = readOutput('git', ['tag', '--list', tag]);
if (existingTag) {
  console.error(`Tag already exists: ${tag}`);
  process.exit(1);
}

run('git', ['tag', '-a', tag, '-m', `Pre-release ${tag}`]);
run('git', ['push', 'origin', tag]);

console.log(`Pre-release tag created and pushed: ${tag}`);
