# PanelMC

Lightweight desktop panel for local Minecraft servers. Built in Rust + Slint so it stays usable on old hardware (Core 2 / Q9550 class, 8 GB RAM, no SSE4.2, no AVX).

Maintainer: [@Q9550xRX570](https://github.com/Q9550xRX570)

Not affiliated with Mojang, Microsoft, PaperMC, or Aternos.

## Requirements

- Windows 10+ or Linux (x86_64 or aarch64)
- [Rust](https://rustup.rs/) (stable)
- A Minecraft server JAR is downloaded from the app (Paper / Purpur / …)
- Java can be downloaded from the Java tab (stored under `runtimes/`, not in git)

### Linux packages (Debian/Ubuntu)

```bash
sudo apt install build-essential pkg-config libfontconfig1-dev libxkbcommon-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libx11-dev libxcursor-dev libxrandr-dev \
  libxi-dev libgl1-mesa-dev zenity
```

`zenity` is only for folder/zip pickers (KDE: `kdialog` also works).

## Run

```bash
cargo run
```

Release:

```bash
cargo run --release
```

On x86_64, `.cargo/config.toml` keeps SSE4.2/AVX off so the same binary can start on a Q9550-class CPU. ARM Linux is unaffected.

## Data (local only)

| Path | What |
| --- | --- |
| `servers/` | Your worlds, plugins, `servers.json`, tokens |
| `runtimes/` | Downloaded JDKs |

These folders are gitignored. Do not commit them.

## Features

- Create / start / stop a local server
- Console with follow-until-scroll
- Settings (RAM, resource pack, gamemode, timezone, …)
- Worlds, players, backups
- Plugin/mod search (Modrinth, Spigot)
- Optional Google Drive / OneDrive backup login

## License

MIT
