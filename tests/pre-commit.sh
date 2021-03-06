#!/bin/sh
set -ev
cargo fmt -- --check
cargo clippy -- -D clippy::all -D unused-imports
cargo test --all-features
