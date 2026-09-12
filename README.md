# GNOME Briefcase

GNOME Briefcase is a native GNOME application for explicit, local, bidirectional folder synchronization. It keeps all synchronization metadata in the portable Briefcase and never writes markers into the Source folder.

## What works

- Create and reopen portable Briefcases.
- Detect new, changed, deleted, and equal files with SHA-256 baselines.
- Copy in either direction using streamed temporary files and atomic replacement.
- Synchronize empty directories and deletions.
- Detect two-sided conflicts and leave them untouched until the user decides.
- Reject overlapping roots, path traversal, and symbolic links.
- Journal operations in SQLite and recover by rescanning after interruption.
- Fast and verified comparison modes.
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
gnome-briefcase
```

For a developer run without installing, compile the schema and point GLib at it:

```sh
glib-compile-schemas data
GSETTINGS_SCHEMA_DIR=data cargo run --features gui
```

## Safety model

A synchronization always starts from a fresh scan. The complete plan exists before any mutation. Conflicts default to Skip, and failed copies stop the transaction before later destructive operations. Briefcase does not merge content, pick the newest timestamp, follow symlinks, run a daemon, or contact a network service.

## Packaging

The same binary is used by native/Meson, Debian, and Flatpak builds. The Flatpak manifest intentionally avoids unrestricted host filesystem access; folders are selected by the user through the desktop portal.

