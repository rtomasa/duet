# Duet

Duet is a native GNOME application for explicit, local, bidirectional folder synchronization. It keeps all synchronization metadata in the portable Target folder and never writes markers into the Source folder.

## What works

- Create and reopen portable sync pairs.
- Detect new, changed, and deleted files using size and modification-time baselines.
- Copy in either direction using streamed temporary files and atomic replacement.
- Synchronize empty directories and deletions.
- Detect two-sided conflicts and leave them untouched until the user decides.
- Reject overlapping roots and path traversal; optionally copy symbolic links as links without following them.
- Journal operations in SQLite and recover by rescanning after interruption. SQLite works from a local temporary copy and atomically updates the portable metadata, so SFTP-mounted Targets do not need to provide SQLite file locking.
- GTK 4/libadwaita interface plus GSettings, AppStream, desktop, and icon metadata.

## Build the synchronization core

```sh
cargo test
```

## Build and run the GNOME application

Install Rust, Meson, GTK 4 development files, and libadwaita development files, then run:

```sh
meson setup build
meson compile -C build
meson install -C build
duet
```

For a developer run without installing, compile the schema and point GLib at it:

```sh
glib-compile-schemas data
GSETTINGS_SCHEMA_DIR=data cargo run --features gui
```

## Safety model

A synchronization always starts from a fresh metadata scan. The complete plan exists before any mutation. Conflicts default to Skip, and failed copies stop the transaction before later destructive operations. Duet does not merge content, pick the newest timestamp, follow symbolic links, run a daemon, or contact a network service. Symbolic links are copied as links by default and can be ignored in Preferences. File changes that preserve both size and the filesystem-reported modification time may not be detected.

## Packaging

The same binary is used by native/Meson and Debian builds. Debian packaging metadata is maintained in `debian/`.
