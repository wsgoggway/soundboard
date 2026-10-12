# KDE Plasma global shortcuts: how user-defined command hotkeys work (M4 research)

Researched 2026-10-12 against the live host: CachyOS, **Plasma 6.7.5**
(`plasmashell 6.7.5`, `kwin 6.7.5`), Wayland session. All statements below are
verified against the KDE sources (GitHub mirrors of invent.kde.org) and the
local D-Bus interface XMLs, not guessed.

## Where shortcuts live

- **`~/.config/kglobalshortcutsrc`** — the shortcut registry (KConfig INI).
  One section per "component". Components are either application/action
  groups (e.g. `[kmix]`) or **service components**: sections named
  `[services][<desktop-file-storage-id>]`.
- In the modern format each service action is a single key with the *current*
  shortcut only, e.g. `_launch=Alt+1` (multiple keys joined by `\t`).
  The old `current,default,friendly-name` triplet format only survives in
  legacy top-level groups and is migrated away by the daemon
  (`GlobalShortcutsRegistry::migrateConfig`).
- Override semantics: an entry is written **only when the current shortcut
  differs from the default**. The default comes from the `.desktop` file's
  `X-KDE-Shortcuts` entry (`KServiceActionComponent::loadSettings`:
  `configGroup.readEntry("_launch", defaultFromDesktopFile)`).

## How a "Command" shortcut executes

For service components the daemon does not signal a running app — it launches
the desktop file itself:
`KServiceActionComponent::emitGlobalShortcutEvent` creates a
`KIO::ApplicationLauncherJob(m_service)` for the `_launch` action. So the
`Exec` line of the desktop file is what runs on keypress.

## Where the daemon finds command desktop files

Two discovery paths (`GlobalShortcutsRegistry::loadSettings` /
`refreshServices` / `detectAppsWithShortcuts`):

1. **`<XDG data dirs>/kglobalaccel/*.desktop`** — parsed into service
   components **only at daemon startup**. Files with `NoDisplay=true` are
   skipped. `refreshServices` only prunes dead entries from this path, it
   never adds new ones at runtime.
2. **Applications with an `X-KDE-Shortcuts` property** — found through
   `KApplicationTrader::query` (ksycoca database, which includes
   `~/.local/share/applications/`), refreshed whenever ksycoca signals
   `databaseChanged` (the daemon connects `KSycoca::databaseChanged` →
   `refreshServices`). `KApplicationTrader::query` filters only
   `OnlyShowIn`/`NotShowIn` (`showInCurrentDesktop`); **`NoDisplay=true` does
   not exclude a service**. Path (2) therefore supports live pickup after a
   `kbuildsycoca6` run, no re-login needed.

## What System Settings itself does (Add New → Command or Script)

`KCMKeys::addCommand` in `plasma-desktop/kcms/keys/kcm_keys.cpp`:

- writes `~/.local/share/applications/net.local.<executable>...desktop` with:
  `Type=Application`, `Name`, `Exec` (with `%` escaped as `%%`),
  `NoDisplay=true`, `StartupNotify=false`,
  `X-KDE-GlobalAccel-CommandShortcut=true`;
- nudges the daemon over D-Bus (`doRegister` + `unregister`) so the new
  component appears immediately;
- the actual keybinding is then assigned in the UI and lands in
  `~/.config/kglobalshortcutsrc` as `[services][<id>.desktop] _launch=...`.

## Why we do NOT write kglobalshortcutsrc directly

The file is owned and rewritten by the shortcut server at runtime (in Plasma
6.4+ Wayland the whole kglobalacceld code runs **inside `kwin_wayland`** — on
this host `org.kde.kglobalaccel` is owned by the `kwin_wayland` PID; the
standalone `/usr/lib/kglobalacceld` binary and
`plasma-kglobalaccel.service` exist but are not running). Editing a
live-managed KConfig file from a second process risks lost updates and
corruption, and a wrong entry can break the user's shortcuts. The D-Bus API
(`org.kde.KGlobalAccel`, `/kglobalaccel`, see
`/usr/share/dbus-1/interfaces/kf6_org.kde.KGlobalAccel.xml`) only registers
actions for *running applications that receive the keypress signal* — it has
no "launch this command" registration; command shortcuts are a file-based
mechanism by design.

## The soundboard `--write` design (safe automatic path)

For each of the first 9 clips (sorted) write ONE self-contained file:

```
~/.local/share/applications/net.local.soundboard.<slug>.desktop
```

```ini
# Managed by "soundboard hotkeys --write". Editing is safe; delete to remove.
[Desktop Entry]
Type=Application
Name=Soundboard: <clip>
Exec=soundboard play <escaped clip name>
NoDisplay=true
StartupNotify=false
X-KDE-GlobalAccel-CommandShortcut=true
X-KDE-Shortcuts=Alt+N
```

- `X-KDE-Shortcuts=Alt+N` is the *default* shortcut, read straight from the
  desktop file — `kglobalshortcutsrc` stays untouched (override entries are
  only needed for non-default keys).
- Discovery goes through ksycoca (path 2 above): `Exec` uses the bare
  `soundboard` command, so it must stay on the user's `PATH`.
- `Exec` escaping per the Desktop Entry Specification: the whole clip name is
  one double-quoted argument; inside quotes `\`, `"`, `` ` ``, `$` are
  backslash-escaped and literal newlines/tabs/CRs use the `\n`/`\t`/`\r`
  sequences; any `%` is doubled to `%%`.
- Idempotent: deterministic file names (slugified clip, `-2`/`-3` on
  collision) and content derived purely from `(clip, key)`; re-runs detect
  unchanged files. Files are only overwritten when they carry our generator
  marker comment; a foreign file with the same name is never touched. Before
  an overwrite the previous content is backed up next to the file with a
  timestamp suffix.

## Activation

- Live pickup (usual case): run `kbuildsycoca6` — the daemon sees the ksycoca
  `databaseChanged` and registers the new shortcuts.
- Guaranteed fallback: log out and back in (the registry loads all paths at
  startup). On sessions where the standalone daemon runs (X11),
  `systemctl --user restart plasma-kglobalaccel.service` also works; on
  Plasma 6.4+ Wayland like this host the server lives in kwin, so do not
  restart that unit expecting a reload.

## Manual path (what the default `soundboard hotkeys` output teaches)

System Settings → Keyboard → Shortcuts → **Add New → Command or Script…**,
paste `soundboard play <clip>`, assign `Alt+N`.
(Russian UI: Параметры системы → Клавиатура → Комбинации клавиш →
«Добавить» → «Команда или сценарий…».)

## Sources

- kglobalacceld daemon: `GlobalShortcutsRegistry` (load/refresh/migrate),
  `KServiceActionComponent` (launch, write/load) —
  <https://github.com/KDE/kglobalacceld> (`src/globalshortcutsregistry.cpp`,
  `src/kserviceactioncomponent.cpp`).
- Shortcuts KCM `addCommand` —
  <https://github.com/KDE/plasma-desktop> (`kcms/keys/kcm_keys.cpp`,
  `kcms/keys/globalaccelmodel.cpp`).
- ksycoca application query semantics —
  <https://github.com/KDE/kservice> (`src/services/kapplicationtrader.cpp`).
- D-Bus surface — local introspection:
  `/usr/share/dbus-1/interfaces/kf6_org.kde.KGlobalAccel.xml`,
  `kf6_org.kde.kglobalaccel.Component.xml`.
- Host facts: `plasmashell --version` → 6.7.5; `busctl --user status
  org.kde.kglobalaccel` → PID of `kwin_wayland`;
  `pacman -Ql kglobalacceld` → `libKGlobalAccelD.so.0` linked into kwin.
