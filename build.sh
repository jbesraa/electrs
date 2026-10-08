#!/usr/bin/env bash
# Build this fork and install it to <root>/bin/electrs.
#
# The bundled RocksDB (librocksdb-sys 0.32.0 -> RocksDB 9.10.0) relies on <cstdint>
# being pulled in transitively by the standard library headers. libstdc++ 16 no longer
# does that, so every translation unit fails with "'uint64_t' has not been declared"
# unless the header is forced. CXXFLAGS must be EXPORTED, not passed inline: cc-rs reads
# it when it spawns the compiler for each unit.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export CXXFLAGS="-include cstdint"
export CARGO_BUILD_BUILD_DIR="${CARGO_BUILD_BUILD_DIR:-$here/../electrs-build}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$here/../electrs-build}"

# --locked: Cargo.lock is committed and includes the vendored bindex, so a build is
# reproducible and fails loudly if the lock drifts from Cargo.toml.
exec cargo install --path "$here" --locked \
    --root "${ELECTRS_ROOT:-$here/..}" \
    -j "${JOBS:-4}"
