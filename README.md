# PanelMC

Native Windows and Linux panel for Paper / Purpur servers. Rust + Slint, GNU GPLv3, no account.

- Website: [panelmc.com](https://panelmc.com)
- Releases: [GitHub Releases](https://github.com/Q9550xRX570/PanelMC/releases)
- Maintainer: [@Q9550xRX570](https://github.com/Q9550xRX570)

The panel is kept small so Java can use the RAM. x86_64 builds do not require SSE4.2 or AVX, so newer PCs and older 64-bit ones can both start it.

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

On x86_64, `.cargo/config.toml` keeps SSE4.2/AVX off so the same binary can start on older 64-bit CPUs as well as new ones. ARM Linux is unaffected.

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

Copyright (C) 2026 Q9550xRX570

PanelMC is free software: you can redistribute it and/or modify it under the terms of the [GNU General Public License v3](LICENSE) as published by the Free Software Foundation.

If you **distribute** PanelMC or a modified version, you must provide the corresponding source under GPLv3. Running the app (hosting a world, installing plugins) does not put those files or the Minecraft client under the GPL.

Slint is used under its GPLv3 option. Other crates (reqwest, serde, …) are MIT/Apache-2.0, which can be included in a GPLv3 program.
