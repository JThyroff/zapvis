# zapvis

A lightweight, keyboard-driven image sequence viewer with support for local and remote (SSH) files.

## Overview

**zapvis** is designed for quickly browsing through sequences of numbered images using a single pattern. Instead of managing folders or playlists, you simply open any image file that matches a configured pattern—the viewer automatically detects the sequence and lets you navigate by frame.

Common use cases:
- Reviewing rendered animation frames
- Inspecting distributed training/testing results
- Browsing remote video breakdowns or scene detection outputs

## Features

- **Pattern-based navigation**: Define patterns like `frame_#####.png` where `#` marks the frame number
- **Local & remote support**: View sequences from your filesystem or over SSH
- **Efficient caching**: Bidirectional preload with configurable radius to keep images ready
- **Non-blocking UI**: Background image loading on separate threads; UI never stalls
- **Persistent SSH**: Single SSH connection reused for all remote operations
- **Configuration**: Patterns are auto-saved and reused

## Building

Requires Rust 1.70+.

```bash
cargo build --release
```

The binary will be at `target/release/zapvis`.

## Usage

### Basic

Open an image file:

```bash
zapvis /path/to/frame_00000.png
```

The viewer will try to match it against patterns in your config. If a match with neighbor evidence is found, the sequence loads.

### With a New Pattern

Specify a pattern inline:

```bash
zapvis /path/to/frame_00000.png --pattern "frame_#####.png"
```

If the pattern matches and neighbors are found, it will be saved to config for future use.

### Remote Files

Use `user@host:/path/to/file` syntax:

```bash
zapvis user@render.server.local:/renders/job_123/frame_00000.png
```

SSH will connect to the server on port 58022 (hardcoded).

### Show Config

View your current patterns and config location:

```bash
zapvis --config
```

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| <kbd>←</kbd> or <kbd>A</kbd> | Previous frame |
| <kbd>→</kbd> or <kbd>D</kbd> | Next frame |
| <kbd>F</kbd> | Toggle fullscreen (OS window maximization, keeps window decorations) |
| <kbd>Esc</kbd> | Quit |

## Configuration

Patterns are stored in a platform-specific config directory:

- **Linux/macOS**: `~/.config/zapvis/zapvis/config.toml`
- **Windows**: `%APPDATA%\zapvis\zapvis\config.toml`

Example `config.toml`:

```toml
patterns = [
    "frame_#####.png",
    "render_####_final.exr",
    "output_###.jpg"
]
```

Patterns are automatically added when you use `--pattern` with a successful match.

## Pattern Rules

- Patterns use `#` as a digit placeholder
- Exactly **one contiguous run** of `#` is supported (current limitation)
- Width is determined by the number of `#` symbols
- Examples:
  - `frame_####.png` → matches `frame_0123.png` (4-digit width)
  - `output_#####.exr` → matches `output_00042.exr` (5-digit width)

## Technical Details

### Architecture

- **UI**: egui/eframe for immediate-mode GUI
- **Image loading**: image crate, decoded in background threads
- **Cache**: Maintains images in [current - radius, current + radius] range
- **SSH**: Custom protocol over persistent shell session (see `persistent_ssh.rs`)
- **Threading**: 
  - Main UI thread (egui)
  - Image decoder thread (waits on load requests)
  - Remote worker thread (owns SSH connection, executes commands serially)

### Remote Protocol

When connecting via SSH, a simple shell loop on the remote end handles three commands:

- `EXISTS <path>` → responds `OK` or `NO`
- `CAT <path>` → responds `OK <bytes>\n<raw_data>` or `NO`
- `QUIT` → exits

This avoids repeated SSH handshakes and keeps the channel open for fast queries.

## Troubleshooting

**"No sequence pattern matched"**
- Ensure your filename follows a pattern in the config
- Try adding a custom pattern with `--pattern`
- Check that at least one neighboring frame exists

**Remote files fail to load**
- Verify SSH connectivity: `ssh -p 58022 user@host ls /path/to/dir`
- Ensure public-key auth is configured (no password prompts)
- Check the server has the `sh` shell available

**Image loads slowly**
- Increase the cache radius in code (adjust `cache_radius` in `main.rs`)
- For remote files, this is limited by network and server responsiveness

## Dependencies

- `egui`/`eframe` – GUI
- `image` – image decoding
- `regex` – pattern matching
- `serde`/`toml` – config serialization
- `clap` – CLI parsing
- `directories` – platform config paths

See `Cargo.toml` for full dependency list.

## License

TBD
