#!/usr/bin/env sh
# Deploys the Tripwire demo setup to testnet under a master account you own.
# Usage: scripts/deploy-testnet.sh <master>.testnet
# See sandbox-tests/README.md, section Testnet.
set -eu
cd "$(dirname "$0")/.."
exec cargo run --quiet -p sandbox-tests --bin deploy -- "$@"
