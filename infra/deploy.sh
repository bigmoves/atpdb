#!/bin/bash
set -euo pipefail

# Deploy latest atpdb binary to production
# Usage: ./deploy.sh [host]
# Example: ./deploy.sh deploy@atpdb.slices.network

HOST="${1:-deploy@atpdb.slices.network}"
BINARY_URL="https://github.com/bigmoves/atpdb/releases/download/latest/atpdb-linux-amd64"

echo "Deploying to $HOST..."

ssh "$HOST" bash -s << 'EOF'
set -euo pipefail

echo "Downloading latest binary..."
curl -fsSL -o /tmp/atpdb-new https://github.com/bigmoves/atpdb/releases/download/latest/atpdb-linux-amd64

echo "Stopping atpdb service..."
sudo systemctl stop atpdb

echo "Replacing binary..."
sudo mv /tmp/atpdb-new /usr/local/bin/atpdb
sudo chmod +x /usr/local/bin/atpdb

echo "Starting atpdb service..."
sudo systemctl start atpdb

echo "Checking status..."
sleep 2
sudo systemctl status atpdb --no-pager

echo "Deploy complete!"
EOF
