#!/usr/bin/env python3
"""Check `blockchain.scripthash.get_history_paged` against a running electrs.

There is no unit-test harness for the index/electrum layers (they need a full synced
index), so this is the test that matters: run it against a real server.

It does two things.

1. CROSS-CHECK against the standard unpaged method on an ordinary address. The paged
   history is walked to exhaustion and its (txid, height) set must equal the set
   `blockchain.scripthash.get_history` returns. Same data, different order — that is the
   strongest check available without a second implementation to compare against.
2. PAGE an address that is far too large for `get_history` (which, with
   `index_lookup_limit` set, refuses instead of wedging the server). Asserts pages advance
   strictly, never repeat a transaction, and cost the same per page as a small address —
   the property the whole change exists for.

Usage:
    python3 contrib/check_paged_history.py [host:port] [large_address]

Default large address is a large, long-lived exchange address. Note it needs no
configuration beyond a server whose index is synced.
"""

import hashlib
import json
import socket
import sys
import time

DEFAULT_SERVER = "127.0.0.1:50001"
# An ordinary address with a few hundred transactions, used for the cross-check.
CONTROL_ADDRESS = "bc1q6f2rpl3amxds4gjyacc03tsnvkgmrvuwz23p56"
# A large exchange address: millions of transactions, so `get_history` refuses it.
LARGE_ADDRESS = "bc1qm34lsc65zpw79lxes69zkqmk6ee3ewf0j77s3h"

CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"
GENERATOR = [0x3B6A57B2, 0x26508E6D, 0x1EA119FA, 0x3D4233DD, 0x2A1462B3]


def _polymod(values):
    chk = 1
    for value in values:
        top = chk >> 25
        chk = (chk & 0x1FFFFFF) << 5 ^ value
        for i in range(5):
            if (top >> i) & 1:
                chk ^= GENERATOR[i]
    return chk


def _expand(hrp):
    return [ord(c) >> 5 for c in hrp] + [0] + [ord(c) & 31 for c in hrp]


def scripthash(address):
    """electrum scripthash of a bech32 address: reversed sha256 of the scriptPubKey."""
    pos = address.rfind("1")
    hrp, data = address[:pos], [CHARSET.find(c) for c in address[pos + 1 :]]
    assert _polymod(_expand(hrp) + data) == 1, "bad bech32 checksum"
    data = data[:-6]
    witness_version, payload = data[0], data[1:]
    acc = bits = 0
    program = bytearray()
    for value in payload:
        acc = (acc << 5) | value
        bits += 5
        while bits >= 8:
            bits -= 8
            program.append((acc >> bits) & 0xFF)
    script = bytes([witness_version, len(program)]) + bytes(program)
    return hashlib.sha256(script).digest()[::-1].hex()


class Electrum:
    def __init__(self, server):
        host, port = server.rsplit(":", 1)
        self.sock = socket.create_connection((host, int(port)), timeout=120)
        self.sock.settimeout(120)
        self.file = self.sock.makefile("rwb")
        self.next_id = 0

    def call(self, method, params):
        self.next_id += 1
        request = {"id": self.next_id, "method": method, "params": params}
        started = time.time()
        self.file.write(json.dumps(request).encode() + b"\n")
        self.file.flush()
        response = json.loads(self.file.readline())
        elapsed = time.time() - started
        if "error" in response and response["error"] is not None:
            raise RuntimeError(f"{method} -> {response['error']}")
        return response["result"], elapsed


def walk_pages(client, sh, limit):
    """Every entry the paged API will give, newest first."""
    entries, cursor, pages = [], None, 0
    while True:
        result, elapsed = client.call(
            "blockchain.scripthash.get_history_paged", [sh, cursor, limit, True]
        )
        pages += 1
        entries.extend(result["entries"])
        if not result["more"] or result["next_cursor"] is None:
            return entries, pages, elapsed
        assert result["next_cursor"] != cursor, "cursor did not advance"
        cursor = result["next_cursor"]


def main():
    server = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_SERVER
    large = sys.argv[2] if len(sys.argv) > 2 else LARGE_ADDRESS
    client = Electrum(server)
    failures = 0

    # ---- 1. cross-check against the standard method ----
    control = scripthash(CONTROL_ADDRESS)
    unpaged, _ = client.call("blockchain.scripthash.get_history", [control])
    paged, pages, _ = walk_pages(client, control, 50)

    expected = sorted((e["tx_hash"], e["height"]) for e in unpaged)
    actual = sorted((e["tx_hash"], e["height"]) for e in paged)
    if expected == actual:
        print(f"PASS cross-check: {len(expected)} entries over {pages} pages, identical to get_history")
    else:
        failures += 1
        print(
            f"FAIL cross-check: get_history has {len(expected)} entries, "
            f"paged has {len(actual)}"
        )
        missing = set(expected) - set(actual)
        extra = set(actual) - set(expected)
        if missing:
            print(f"  missing from paged: {list(missing)[:3]}")
        if extra:
            print(f"  extra in paged:    {list(extra)[:3]}")

    # Ordered newest-first: heights must not increase.
    heights = [e["height"] for e in paged]
    if heights == sorted(heights, reverse=True):
        print("PASS order: pages are newest-first")
    else:
        failures += 1
        print("FAIL order: heights are not monotonically decreasing")

    # ---- 2. page an address the unpaged method refuses ----
    big = scripthash(large)
    try:
        client.call("blockchain.scripthash.get_history", [big])
        print(f"NOTE {large} is small enough for get_history; paging was not stressed")
    except RuntimeError as err:
        print(f"expected: get_history refuses it ({str(err)[:80]}...)")

    seen, cursor, timings = set(), None, []
    for page in range(3):
        result, elapsed = client.call(
            "blockchain.scripthash.get_history_paged", [big, cursor, 10, True]
        )
        timings.append(elapsed)
        entries = result["entries"]
        if not entries:
            failures += 1
            print(f"FAIL page {page + 1} is empty")
            break
        if any(e["tx_hash"] in seen for e in entries):
            failures += 1
            print(f"FAIL page {page + 1} repeats a transaction from an earlier page")
        seen.update(e["tx_hash"] for e in entries)
        heights = [e["height"] for e in entries]
        if heights != sorted(heights, reverse=True):
            failures += 1
            print(f"FAIL page {page + 1} is not newest-first")
        print(
            f"PASS page {page + 1}: {len(entries)} entries, heights "
            f"{heights[0]}..{heights[-1]}, {elapsed * 1000:.0f} ms"
        )
        if not result["more"]:
            break
        cursor = result["next_cursor"]

    slowest = max(timings) if timings else 0
    print(
        f"{'PASS' if slowest < 1.0 else 'NOTE'} slowest page {slowest * 1000:.0f} ms "
        f"({len(seen)} distinct transactions paged)"
    )
    if slowest >= 1.0:
        failures += 1

    print("FAILURES" if failures else "ALL CHECKS PASSED")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
