# What this fork changes, and why

Base: **electrs 0.12.0** exactly as published on crates.io (`cargo install electrs
--version 0.12.0`), upstream commit `37501cc4b94aea99e50670a6524fa3ad4ac9aabb`.
`vendor/bindex` is **bindex 0.1.2**, also exactly as published, plus the changes described
below. Both are vendored so that a checkout of this repository builds the same code the
deployed binary runs, rather than depending on a patch someone applied by hand.

Two problems motivated the fork. Both are about the same address history path.

## 1. `index_lookup_limit` was dead code

`Config::index_lookup_limit` is parsed from the config file and then read **nowhere**: the
name appears only in `config.rs`. Its own help text claims it caps "the number of
transactions to lookup before server to get stuck", so setting it looks like protection
that is not there.

Without it, `blockchain.scripthash.get_history` on a large address walks the entire
history. Measured on an exchange address in production:

    index reads    859 MB
    RSS           1.05 GB
    threads       47, process alive and idle-looking
    effect        server.ping, headers.subscribe and estimatefee ALL time out;
                  every other client is starved until electrs is restarted

The process never crashes — it stops answering, which is why this presents as "some
addresses fail" rather than as a server that died.

**Change** (`src/tracker.rs`, `src/status.rs`): the value is threaded into the history
walk, and the walk refuses on the first entry past the limit, *before* reading or
decoding that entry. The refusal is a normal JSON-RPC error, so a client can tell it
apart from a timeout.

| | before | after |
|---|---|---|
| over-limit address | never returns; everyone starved | refused in ~1.5-3.8 s |
| other clients during it | starved for minutes | a concurrent ping waits ~4 s, then normal |

## 2. History had no pagination

Electrum has no paging: `get_history` returns the whole history or nothing. That leaves a
large address with only two bad options — an enormous response, or (with change 1) a flat
refusal. For a 2.9M-transaction address the first option is not survivable and the second
means the address cannot be browsed at all.

**Change**: a new method, additive and deliberately non-standard.

    blockchain.scripthash.get_history_paged

    params   [scripthash]
             [scripthash, cursor]
             [scripthash, cursor, limit]
             [scripthash, cursor, limit, newest_first]
             cursor        opaque u32; the `next_cursor` of the previous page
             limit         1..1000, default 100
             newest_first  default true
    result   {"entries": [{"tx_hash": "...", "height": 123}, ...],
              "next_cursor": 456,
              "more": true}

Why it is cheap: the script-hash index is keyed `(prefix ‖ txnum)` with `txnum`
big-endian, so lexicographic order is chronological order. A page is therefore a *seek*
plus `limit` body reads — cost independent of the size of the history — while
`locations_by_scripthash` collects every matching entry before the caller can use any of
them.

Two details that are not obvious:

- **Candidates must be post-filtered.** The index keys a script hash by its first **8
  bytes** only (`Prefix::LEN`), and bindex documents that lookups "require
  post-filtering". A page therefore re-checks each candidate: an output must pay the
  script, or an input must spend an output that does. The input side needs the prevout
  script, which a transaction does not carry, so it resolves the prevout through the
  index — and only when the output side did not already match, so a funding transaction
  costs no extra lookups.
- **The cursor advances over candidates, not over the entries kept.** A page whose
  candidates were all filtered out still moves the caller forward, so a prefix shared with
  another script cannot be paged forever.

`blockchain.scripthash.get_history` is untouched, so wallets are unaffected: they do not
know this method and cannot be broken by it.

## Changes in `vendor/bindex`

    src/index/mod.rs   TxNum::{MAX, from_u32, to_u32}         (build a seek position)
    src/db.rs          DB::scan_by_script_hash_page           (limited, directional scan)
    src/chain.rs       IndexedChain::script_hash_page         (page of (txnum, height))
                       IndexedChain::location_by_txnum        (body lookup by position)
                       ScriptHashPageEntry                    (the entry type)

`scan_by_script_hash_page` is the same seek as the existing
`scan_by_script_hash`, with a direction and a `limit` that stops the RocksDB iterator
early. It also removes the residual cost of change 1: a refusal now stops after
`limit + 1` candidates instead of collecting millions of `TxNum`s first.

## Building

    ./build.sh

The script exports `CXXFLAGS=-include cstdint`, which GCC 16 needs for the bundled
RocksDB 9.10.0 (libstdc++ 16 no longer pulls in `<cstdint>` transitively, so every
translation unit otherwise fails with `'uint64_t' has not been declared`). `CXXFLAGS`
must be exported rather than passed inline, because cc-rs reads it per spawned compiler.

`Cargo.lock` is committed and kept in sync with the `[patch.crates-io]` entry, so
`--locked` builds work.
