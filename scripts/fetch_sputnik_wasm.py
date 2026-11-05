#!/usr/bin/env python3
"""Fetch the pinned Sputnik DAO v2 wasm from the mainnet factory and verify it.

The sandbox tests deploy this file. It is the code that `sputnik-dao.near` uses for new DAOs,
built from near-daos/sputnik-dao-contract commit 2b0f34a. The script checks that the factory
still lists that commit for the pinned hash and that the bytes hash to it.

Usage: python3 scripts/fetch_sputnik_wasm.py [output path]
"""

import base64
import hashlib
import json
import sys
import urllib.request

FACTORY = "sputnik-dao.near"
CODE_HASH = "DtHZA1mUmGYeScEaUoUZYQGGFHSMpnjMCvkr5U9hJXuF"
COMMIT = "2b0f34a3860460f285307ec2ccf36345acbe05a2"
RPCS = ["https://rpc.mainnet.near.org", "https://free.rpc.fastnear.com"]
DEFAULT_OUT = "sandbox-tests/res/sputnikdao2.wasm"

B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58encode(data: bytes) -> str:
    n = int.from_bytes(data, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = B58[r] + out
    return "1" * (len(data) - len(data.lstrip(b"\0"))) + out


def view(method: str, args: dict) -> bytes:
    payload = json.dumps({
        "jsonrpc": "2.0",
        "id": "tripwire",
        "method": "query",
        "params": {
            "request_type": "call_function",
            "finality": "final",
            "account_id": FACTORY,
            "method_name": method,
            "args_base64": base64.b64encode(json.dumps(args).encode()).decode(),
        },
    }).encode()
    last_error = None
    for rpc in RPCS:
        try:
            request = urllib.request.Request(
                rpc, data=payload, headers={"Content-Type": "application/json"}
            )
            with urllib.request.urlopen(request, timeout=60) as response:
                body = json.load(response)
            if "error" in body:
                raise RuntimeError(body["error"])
            return bytes(body["result"]["result"])
        except Exception as error:  # try the next endpoint
            last_error = error
    raise SystemExit(f"all RPC endpoints failed: {last_error}")


def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_OUT
    metadata = dict(json.loads(view("get_contracts_metadata", {})))
    entry = metadata.get(CODE_HASH)
    if entry is None or not entry.get("commit_id", "").startswith(COMMIT):
        raise SystemExit(f"factory metadata for {CODE_HASH} does not name commit {COMMIT}: {entry}")
    code = view("get_code", {"code_hash": CODE_HASH})
    digest = b58encode(hashlib.sha256(code).digest())
    if digest != CODE_HASH:
        raise SystemExit(f"hash mismatch: got {digest}, expected {CODE_HASH}")
    with open(out, "wb") as f:
        f.write(code)
    print(f"{out}: {len(code)} bytes, sha256 (base58) {digest}, version {entry.get('version')}")


if __name__ == "__main__":
    main()
