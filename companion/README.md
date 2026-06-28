# Photon Companion (GNOME)

A native **GTK4 + libadwaita** desktop client for a self-hosted [Photon](../)
photo library. Sign in to your server, browse your timeline, and upload photos —
in a clean, GNOME-HIG-compliant window.

## Features

- libadwaita UI (adaptive window, header bar, primary menu, About dialog)
- Sign in with email **or** username + password (matches the Photon web app)
- Timeline browsing — thumbnails in a responsive grid, loaded lazily over HTTP
- Upload from a native file chooser (sends to `POST /api/uploads/raw`)
- Sign out / switch server

Networking runs on a background **tokio** runtime; results are marshalled back to
the GTK main thread via `async-channel`, so the UI never blocks.

## Build & run (native)

Requires the GNOME dev stack:

```sh
# Fedora:        sudo dnf install gtk4-devel libadwaita-devel
# Debian/Ubuntu: sudo apt install libgtk-4-dev libadwaita-1-dev
# Arch:          sudo pacman -S gtk4 libadwaita

cd companion
cargo run --release
```

Then point it at your server (default `http://localhost:3000`) and sign in
(demo: `alice` / `alice`).

## Install into GNOME (app grid + icon)

```sh
install -Dm755 target/release/photon-companion ~/.local/bin/photon-companion
install -Dm644 data/app.photon.Companion.desktop      ~/.local/share/applications/app.photon.Companion.desktop
install -Dm644 data/app.photon.Companion.metainfo.xml ~/.local/share/metainfo/app.photon.Companion.metainfo.xml
install -Dm644 data/icons/hicolor/scalable/apps/app.photon.Companion.svg \
    ~/.local/share/icons/hicolor/scalable/apps/app.photon.Companion.svg
update-desktop-database ~/.local/share/applications 2>/dev/null
gtk-update-icon-cache -f -t ~/.local/share/icons/hicolor 2>/dev/null || true
```

## GNOME Shell top-bar extension (icon + menu by the clock/battery)

`shell-extension/` adds a panel indicator with a quick menu (open the app, open
the web library, server status):

```sh
cp -r shell-extension/photon-companion@photon.app \
    ~/.local/share/gnome-shell/extensions/
# Reload GNOME Shell: log out/in (Wayland) or Alt+F2 → r (X11), then:
gnome-extensions enable photon-companion@photon.app
```

## Build as a Flatpak (recommended for GNOME)

```sh
flatpak install flathub org.gnome.Platform//47 org.gnome.Sdk//47 \
    org.freedesktop.Sdk.Extension.rust-stable
flatpak-builder --user --install --force-clean build build-aux/app.photon.Companion.json
flatpak run app.photon.Companion
```

## Layout

```
companion/
├── Cargo.toml
├── src/
│   ├── main.rs        # Adw application, window, login + library views
│   ├── api.rs         # Photon REST client (login, timeline, upload, blobs)
│   └── runtime.rs     # shared tokio runtime
├── data/
│   ├── app.photon.Companion.desktop       # app launcher entry
│   └── app.photon.Companion.metainfo.xml  # AppStream metadata
└── build-aux/
    └── app.photon.Companion.json          # Flatpak manifest
```

## Roadmap

- Background auto-backup of a watched folder (the headline feature)
- Album view + per-photo detail / download of originals
- Native notifications on import completion
- Store the session token in the libsecret keyring
