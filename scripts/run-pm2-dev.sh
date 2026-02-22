#!/usr/bin/env bash

set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "Usage: $0 <pm2-app-name> <npm-script>"
  exit 1
fi

app_name="$1"
npm_script="$2"

cleanup() {
  pm2 delete "$app_name" >/dev/null 2>&1 || true
}

pm2 delete "$app_name" >/dev/null 2>&1 || true
pm2 start npm --name "$app_name" --time -- run "$npm_script" >/dev/null

echo "[pm2] $app_name started. Ctrl+C to stop and remove it from pm2."
echo "[pm2] For memory metrics, run: pm2 monit"

trap cleanup INT TERM EXIT
pm2 logs "$app_name" --lines 200
