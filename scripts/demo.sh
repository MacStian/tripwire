#!/usr/bin/env sh
# The three invoice demo: a routine invoice paid at once, a large one sent to a DAO vote and
# a poisoned one that cannot redirect the payment.
# Usage: scripts/demo.sh            a fresh local node, no setup or keys needed
#        scripts/demo.sh testnet    the deployment in deployments/testnet.json
# See README.md, section Demo.
set -eu
cd "$(dirname "$0")/.."
exec cargo run --quiet -p sandbox-tests --bin demo -- "$@"
