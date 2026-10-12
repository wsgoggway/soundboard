# ADR 0002: KDE hotkeys via generated .desktop files

Date: 2026-10-12. Status: accepted (M4). Research base: `docs/kde-hotkeys.md`.

## Context

Clips must be playable from anywhere (conferencing app in focus, game
fullscreen) with a keypress. The host is CachyOS with KDE Plasma 6.7.5 on a
Wayland session.

Verified facts about Plasma 6 global shortcuts (see `docs/kde-hotkeys.md` for
sources and D-Bus evidence):

- On Plasma 6.4+ Wayland **kglobalacceld runs inside `kwin_wayland`** (the
  standalone daemon binary and `plasma-kglobalaccel.service` exist but are
  not running). There is no separate daemon process to talk to.
- The D-Bus API (`org.kde.KGlobalAccel`) only registers actions for *running
  applications that receive the keypress signal*; it has **no "launch this
  command" registration**. Command shortcuts are a file-based mechanism by
  design.
- `~/.config/kglobalshortcutsrc` is the shortcut registry, but it is owned
  and rewritten live by the shortcut server. Editing it from a second
  process risks lost updates and corruption, and a wrong entry can break the
  user's existing shortcuts.
- What System Settings itself does for "Add New → Command or Script" is the
  model to copy: it writes a self-contained `.desktop` file with
  `X-KDE-GlobalAccel-CommandShortcut=true` into
  `~/.local/share/applications/`, and the daemon picks command shortcuts up
  through **ksycoca** (which includes the user applications directory and
  supports live refresh after `kbuildsycoca6`). The default shortcut comes
  straight from the file's `X-KDE-Shortcuts` entry — no
  `kglobalshortcutsrc` entry is needed unless the user rebinds the key.

## Decision

`soundboard hotkeys --write` generates one self-contained desktop file per
clip into the user applications directory:

```
~/.local/share/applications/net.local.soundboard.<slug>.desktop
```

with `Type=Application`, `NoDisplay=true`, `StartupNotify=false`,
`X-KDE-GlobalAccel-CommandShortcut=true`, `X-KDE-Shortcuts=Alt+N` (the
default binding) and `Exec=soundboard play "<clip>"` (escaped per the
Desktop Entry Specification: quoted argument, `\`, `"`, `` ` ``, `$`
backslash-escaped, `\n`/`\t`/`\r` sequences, `%` doubled).

Safety rules, enforced by `src/hotkeys.rs`:

- **Written only by soundboard**: the first line is a generator marker
  (`# Managed by "soundboard hotkeys --write"...`). A same-named file
  *without* the marker is never touched (skipped with a warning).
- **Backups**: an existing generated file is backed up next to itself
  (`<name>.bak.<millis-timestamp>`) before an overwrite.
- **Idempotent**: file names and content are deterministic functions of
  `(clip, key)`; unchanged files are not rewritten and produce no backups,
  so re-running `--write` is a no-op.
- **kglobalshortcutsrc is never modified** by soundboard.
- Without `--write`, the command only prints the clip → key → command table
  plus manual registration steps for System Settings.

## Alternatives considered

- **Edit `~/.config/kglobalshortcutsrc` directly**: rejected — the file is
  live-managed by the daemon running inside `kwin_wayland`; concurrent
  writes risk lost updates/corruption, and malformed entries can take down
  the user's whole shortcut set.
- **Manual-only** (System Settings → Add New → Command or Script): safe and
  the officially supported path, but tedious for nine clips; kept as the
  printed fallback and the mental model for the generated files.
- **Tauri/Electron-style in-app global hotkeys**: would require a resident
  soundboard process plus a registration API that KDE does not offer for
  command shortcuts (the D-Bus API signals running apps only). The
  soundboard is deliberately daemon-free — `soundboard play` exits right
  after spawning `pw-cat`.

## Consequences

- **Activation is not instant**: run `kbuildsycoca6` (the daemon follows the
  ksycoca `databaseChanged` signal); the guaranteed fallback is re-login.
  The CLI prints both after writing. No D-Bus nudge is attempted (System
  Settings has one; calling it is out of scope and version-fragile).
- **Only the first 9 clips** (sorted by name) get keys, `Alt+1`…`Alt+9`;
  clips beyond that are listed as unmapped in the table (the Alt+digit
  space is what Plasma users expect; extending to other modifiers is future
  work).
- **Slug collisions** are resolved deterministically: the slug is the
  lowercased ASCII-alphanumeric run of the clip name (everything else
  collapses to `-`, an empty result becomes `clip`), and colliding slugs
  get `-2`, `-3`, … suffixes. Cyrillic-only names therefore slugify to
  `clip`, `clip-2`, …
- **The binary must stay on `PATH`**: `Exec` uses the bare `soundboard`
  command, resolved by the launching session.
- Users may edit or delete generated files freely (the marker says so);
  deleting a file removes its shortcut after the next ksycoca refresh.
- Files with `NoDisplay=true` are invisible in app menus and launchers (as
  intended); the ksycoca-based discovery path does not exclude them.
