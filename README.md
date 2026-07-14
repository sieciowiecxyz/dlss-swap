# dlss-swap

`dlss-swap` is a small Linux wrapper for Steam and Proton that pins a chosen
NVIDIA DLSS Super Resolution DLL before launching a game.

It solves one specific problem: keeping a known DLSS version and render preset
in use even when a game ships another DLL. The wrapper finds the game's
`nvngx_dlss.dll`, preserves the original, installs the selected pinned version,
configures DXVK-NVAPI, and then replaces itself with the game process.

## Why this exists

- Select a DLSS render preset per game from Steam Launch Options.
- Switch between the legacy DLSS 3.7.0 CNN model and DLSS 310.7.0 Transformer
  models.
- Avoid hashing an unchanged 50+ MB DLL on every launch.
- Preserve the game's original DLL for recovery.
- Keep the implementation inspectable: one dependency, no daemon, no GUI, and
  no background updater.

## Supported presets

| Preset | Aliases | Pinned DLL | Intended use |
|---|---|---|---|
| E | `cnn`, `competitive`, `fast` | DLSS 3.7.0 | Lower processing cost and very high frame rates |
| K | `quality`, `default`, `transformer` | DLSS 310.7.0 | DLAA, Quality, and Balanced modes |
| M | `performance`, `low-res` | DLSS 310.7.0 | Performance mode and fast motion |
| L | `ultra-performance`, `ultra-low-res` | DLSS 310.7.0 | Very low internal resolutions |

The preset selects the reconstruction model. It does not change the scaling
mode selected in the game.

## Requirements

- Linux
- Steam with Proton
- A game that contains `nvngx_dlss.dll`
- DXVK-NVAPI support for the DLSS override variables
- Rust 2024 toolchain to build from source
- `curl` and `unzip`

## Build and install

```sh
git clone https://github.com/sieciowiecxyz/dlss-swap.git
cd dlss-swap
cargo build --release
install -Dm755 target/release/dlss-swap ~/.local/bin/dlss-swap
```

On first use of a preset, the program downloads the matching official Windows
demo archive from the [NVIDIA/DLSS releases](https://github.com/NVIDIA/DLSS/releases),
extracts `nvngx_dlss.dll`, verifies its pinned SHA-256 hash, and stores it in:

```text
~/.local/share/dlls-swap/3.7.0/nvngx_dlss.dll
~/.local/share/dlls-swap/310.7.0/nvngx_dlss.dll
```

When `XDG_DATA_HOME` is set, `$XDG_DATA_HOME/dlls-swap/` is used instead. The
historical `dlls-swap` storage spelling is retained for compatibility.

No NVIDIA binaries are included in this repository. An existing verified DLL
is reused without network access.

## Steam Launch Options

Select a preset and launch the game:

```text
~/.local/bin/dlss-swap --preset quality -- %command%
```

Other examples:

```text
~/.local/bin/dlss-swap --preset cnn -- %command%
~/.local/bin/dlss-swap --preset performance -- %command%
~/.local/bin/dlss-swap --preset ultra-performance -- %command%
```

Preset L remains the default for compatibility when `--preset` is omitted.

## Commands

```sh
dlss-swap status
dlss-swap restore
dlss-swap --dry-run --preset quality -- command
dlss-swap --preset help
dlss-swap --help
```

`status` shows the discovered game DLL, selected pinned version, SHA-256 state,
cache status, and backup status.

`restore` reinstalls the one-time backup without deleting it, so the original
remains recoverable.

Launch, `status`, and `restore` require `STEAM_COMPAT_INSTALL_PATH`. Steam sets
it automatically for Proton launch commands.

## How it works

1. Resolve and validate `STEAM_COMPAT_INSTALL_PATH`.
2. Reuse a cached DLL path when it is still valid; otherwise scan the game
   directory.
3. Reuse a cached hash only when device, inode, size, and modification time are
   unchanged.
4. Download a missing pinned DLL from the matching official NVIDIA release and
   verify it against its expected SHA-256 hash.
5. Create a one-time backup when one does not already exist.
6. Replace the target through a same-directory temporary file and atomic rename.
7. Set the DXVK-NVAPI preset override and `exec` the game command.

Cache files live in `$XDG_CACHE_HOME/dlls-swap/` or `~/.cache/dlls-swap/`.
Cache write failures are warnings and never prevent a game launch.

## NVIDIA binaries and licensing

NVIDIA DLL files are not part of this repository or its releases. Obtain them
from an official NVIDIA source and review the applicable NVIDIA RTX SDK license
before use or distribution.

NVIDIA, GeForce, RTX, and DLSS are trademarks of NVIDIA Corporation. This
project is independent and is not sponsored or endorsed by NVIDIA.
