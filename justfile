default:
    @just --list

build:
    cargo build

test:
    cargo test

lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

run:
    cargo run
