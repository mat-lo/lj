# lj

A CLI tool to download magnet links via Real-Debrid with background downloading support.

## Features

- Download magnet links through Real-Debrid's servers
- Background downloads that survive SSH disconnects
- File selection for multi-file torrents
- Progress tracking with `lj dl`
- Downloads to current directory

## Installation

### With Nix

```bash
nix run github:user/lj -- <magnet>
```

### From source

```bash
cargo build --release
cp target/release/lj /usr/local/bin/
```

## Usage

```bash
# Download a magnet link (downloads to current directory)
lj "magnet:?xt=urn:btih:..."

# Check download progress
lj dl

# Set or update API key
lj set-key
```

## First Run

On first run (or if no API key is configured), lj will prompt for your Real-Debrid API key.

Get your API key from: https://real-debrid.com/apitoken

The key is stored in `~/.config/lj/api_key` (or equivalent on your OS).

## How It Works

1. Submits magnet to Real-Debrid
2. Waits for file list
3. For single file: auto-downloads
4. For multiple files: shows selection menu
5. Waits for Real-Debrid to cache/process
6. Spawns background download processes
7. Downloads complete even after terminal closes

## Commands

### `lj <magnet>`

Downloads from a magnet link. Files are saved to the current directory.

### `lj dl`

Shows all downloads with status, progress, and speed. Interactive commands:
- `c <n>` - Cancel download #n
- `r <n>` - Remove completed/failed download #n
- `C` - Clear all completed/failed/cancelled
- `q` - Quit

### `lj set-key`

Interactively set or update your Real-Debrid API key.

## Configuration

Config files are stored in:
- macOS: `~/Library/Application Support/lj/`
- Linux: `~/.config/lj/`

Files:
- `api_key` - Your Real-Debrid API token
- `downloads/` - Per-download state files

## Environment Variables

- `RD_API_TOKEN` - Real-Debrid API key (overrides config file)

## License

MIT
