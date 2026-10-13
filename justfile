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

# Install the desktop entry + app icon into ~/.local (hicolor), refresh caches
install-desktop:
    mkdir -p ~/.local/share/icons/hicolor/scalable/apps \
             ~/.local/share/icons/hicolor/256x256/apps \
             ~/.local/share/applications
    cp assets/icons/org.soundboard.App.svg ~/.local/share/icons/hicolor/scalable/apps/
    cp assets/icons/org.soundboard.App.png ~/.local/share/icons/hicolor/256x256/apps/
    cp assets/org.soundboard.App.desktop ~/.local/share/applications/
    kbuildsycoca6 2>/dev/null || true
    gtk-update-icon-cache -f ~/.local/share/icons/hicolor 2>/dev/null || true

# Decode-audit clips via pw-cat (PLAYS each clip ~2s — audible!)
clips-check DIR="$HOME/Music/soundboard-clips":
    #!/bin/sh
    printf '%-5s %s\n' VERDICT CLIP
    for f in "$DIR"/*; do
        [ -f "$f" ] || continue
        err=$(timeout 2 pw-cat --playback --target soundboard "$f" 2>&1)
        rc=$?
        if [ "$rc" -eq 0 ] || [ "$rc" -eq 124 ]; then
            printf '%-5s %s\n' PASS "$(basename "$f")"
        else
            printf '%-5s %s: %s\n' FAIL "$(basename "$f")" "$(printf '%s' "$err" | head -1)"
        fi
    done
