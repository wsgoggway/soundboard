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

# Copy bundled meme clips into the user's clips directory (no-clobber)
clips-install:
    mkdir -p ~/Music/soundboard-clips
    cp -n clips/* ~/Music/soundboard-clips/
