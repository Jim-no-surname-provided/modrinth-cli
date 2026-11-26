# modrinth-cli

`modrinth-cli` is a small Rust CLI for searching, installing, and updating Modrinth mods directly from a Minecraft instance directory.

The compiled executable is named `mod`, so the normal workflow looks like:

```bash
mod search sodium
mod install simple voice chat
mod update all
```

## What it does

- Searches Modrinth with fuzzy matching# modrinth-cli

`modrinth-cli` is a small Rust CLI for searching, installing, and updating Modrinth mods directly from a Minecraft instance directory.

The compiled executable is named `mod`, so the normal workflow looks like:

```bash
mod search sodium
mod install simple voice chat
mod update all
```

## What it does

- Searches Modrinth with fuzzy matching
- Installs compatible mods for the detected loader and Minecraft version
- Installs required Modrinth dependencies automatically when they can be resolved by project ID
- Updates one installed mod or every installed Modrinth-backed mod
- Offers an interactive terminal UI for live search and multi-select install
- Caches Modrinth API responses locally with configurable retention

## Supported environment detection

The CLI starts from your current directory and walks upward until it finds a Minecraft root or `mods/` directory. It then detects loader and Minecraft version from the local installation.

It currently understands:

- plain `mods/` directories
- roots containing `mods/`
- nested `minecraft/mods/` layouts
- Fabric server jar layouts
- `.fabric/server` and `.fabric/remappedJars`
- Fabric, Quilt, Forge, and NeoForge library layouts
- Prism Launcher / MultiMC style `mmc-pack.json` manifests

By default, `search`, `install`, and `update` are filtered to the detected loader and Minecraft version for the current instance.

## Install

### Run without installing globally

```bash
cargo run -- search sodium
```

### Build a release binary

```bash
cargo build --release
./target/release/mod --help
```

### Install into Cargo's bin directory

```bash
cargo install --path .
mod --help
```

## Usage

### Search

```bash
mod search sodium
mod s voice chat
mod search --downloads sodium
mod search --title sodium
mod search --slug sodium
mod search --global sodium
```

- `--downloads` sorts by download count
- `--title` sorts alphabetically by title
- `--slug` sorts alphabetically by slug
- `--best-match` uses fuzzy ranking and is the default
- `--global` disables local loader/version filtering

### Install

```bash
mod install sodium
mod i simple voice chat
```

Install uses fuzzy matching when the exact slug is not found first. If the selected version declares required dependencies through Modrinth project metadata, those dependencies are installed before the main project.

### Interactive installer

```bash
mod tui
mod dynamic-installer
```

This opens a ratatui-based terminal UI for live searching and multi-select install.

### List installed mods

```bash
mod list
```

Run to see installed mods, their versions, and whether they have available updates compatible with the detected Minecraft version and loader. JARs that Modrinth can’t identify appear by filename with `Unknown` as their version.

### Update

```bash
mod update sodium
mod u sodium
mod update all
```

`mod update all` scans installed jars, matches them back to Modrinth by SHA-1, and replaces only the mods that have a newer compatible version available.

### Cache

```bash
mod cache
mod cache clear
mod cache settings
```

The HTTP cache defaults to:

- 64 MB size limit
- 12 hour TTL
- oldest-first eviction when the size limit is exceeded

The cache settings screen lets you toggle caching per endpoint group, including search results, project metadata, project versions, installed-version lookups, latest-version lookups, and game-version tags.

## Environment variables

The CLI supports a few environment overrides:

- `MODRINTH_API_BASE`: override the Modrinth API base URL
- `MODRINTH_CACHE_DIR`: override the cache root directory
- `MODRINTH_CACHE_SETTINGS_PATH`: override the cache settings JSON path
- `MODRINTH_CACHE_LIMIT_MB`: override the cache size limit in MB
- `MODRINTH_CACHE_TTL_SECS`: override the cache TTL in seconds

Default cache roots:

- macOS: `~/Library/Caches/modrinth-cli`
- Linux: `${XDG_CACHE_HOME}/modrinth-cli` or `~/.cache/modrinth-cli`
- Windows: `%LOCALAPPDATA%/modrinth-cli`

## Development

```bash
cargo fmt
cargo test
cargo build
```

The package name is `modrinth-cli`, but the binary name is `mod`.

- Installs compatible mods for the detected loader and Minecraft version
- Installs required Modrinth dependencies automatically when they can be resolved by project ID
- Updates one installed mod or every installed Modrinth-backed mod
- Offers an interactive terminal UI for live search and multi-select install
- Caches Modrinth API responses locally with configurable retention

## Supported environment detection

The CLI starts from your current directory and walks upward until it finds a Minecraft root or `mods/` directory. It then detects loader and Minecraft version from the local installation.

It currently understands:

- plain `mods/` directories
- roots containing `mods/`
- nested `minecraft/mods/` layouts
- Fabric server jar layouts
- `.fabric/server` and `.fabric/remappedJars`
- Fabric, Quilt, Forge, and NeoForge library layouts
- Prism Launcher / MultiMC style `mmc-pack.json` manifests

By default, `search`, `install`, and `update` are filtered to the detected loader and Minecraft version for the current instance.

## Install

### Run without installing globally

```bash
cargo run -- search sodium
```

### Build a release binary

```bash
cargo build --release
./target/release/mod --help
```

### Install into Cargo's bin directory

```bash
cargo install --path .
mod --help
```

## Usage

### Search

```bash
mod search sodium
mod s voice chat
mod search --downloads sodium
mod search --title sodium
mod search --slug sodium
mod search --global sodium
```

- `--downloads` sorts by download count
- `--title` sorts alphabetically by title
- `--slug` sorts alphabetically by slug
- `--best-match` uses fuzzy ranking and is the default
- `--global` disables local loader/version filtering

### Install

```bash
mod install sodium
mod i simple voice chat
```

Install uses fuzzy matching when the exact slug is not found first. If the selected version declares required dependencies through Modrinth project metadata, those dependencies are installed before the main project.

### Interactive installer

```bash
mod tui
mod dynamic-installer
```

This opens a ratatui-based terminal UI for live searching and multi-select install.

### Update

```bash
mod update sodium
mod u sodium
mod update all
```

`mod update all` scans installed jars, matches them back to Modrinth by SHA-1, and replaces only the mods that have a newer compatible version available.

### Cache

```bash
mod cache
mod cache clear
mod cache settings
```

The HTTP cache defaults to:

- 64 MB size limit
- 12 hour TTL
- oldest-first eviction when the size limit is exceeded

The cache settings screen lets you toggle caching per endpoint group, including search results, project metadata, project versions, installed-version lookups, latest-version lookups, and game-version tags.

## Environment variables

The CLI supports a few environment overrides:

- `MODRINTH_API_BASE`: override the Modrinth API base URL
- `MODRINTH_CACHE_DIR`: override the cache root directory
- `MODRINTH_CACHE_SETTINGS_PATH`: override the cache settings JSON path
- `MODRINTH_CACHE_LIMIT_MB`: override the cache size limit in MB
- `MODRINTH_CACHE_TTL_SECS`: override the cache TTL in seconds

Default cache roots:

- macOS: `~/Library/Caches/modrinth-cli`
- Linux: `${XDG_CACHE_HOME}/modrinth-cli` or `~/.cache/modrinth-cli`
- Windows: `%LOCALAPPDATA%/modrinth-cli`

## Development

```bash
cargo fmt
cargo test
cargo build
```

The package name is `modrinth-cli`, but the binary name is `mod`.
