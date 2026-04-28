# PHPStan for Zed

A [Zed](https://zed.dev) extension that surfaces [PHPStan](https://phpstan.org) static-analysis diagnostics inline in your PHP files — without depending on Composer packages, Node-based bridges, or any other PHP language server.

## Features

- Inline PHPStan diagnostics on save with zero configuration in projects that already ship PHPStan via Composer.
- Automatic phar download from PHPStan's GitHub releases for projects that don't ship PHPStan locally.
- Standalone — does not interfere with `intelephense`, `phpactor`, or any other PHP tooling you have installed.

## Requirements

- **PHP 8.0 or newer** must be available on your `PATH`. PHPStan is a PHP tool, so this is unavoidable.

That's it. The extension downloads its own LSP bridge binary and (if needed) PHPStan itself.

## How PHPStan is Located

The extension resolves PHPStan in this order:

1. `vendor/bin/phpstan` inside your project (Composer-installed).
2. `phpstan` on your `PATH` (e.g. a global `composer global require`).
3. A user-pinned version from PHPStan's GitHub releases.
4. The latest stable phar from PHPStan's GitHub releases.

The first match wins, so a project-local install is always preferred. This keeps PHPStan's project-specific extensions (Larastan, phpstan-strict-rules, …) loaded correctly.

## Configuration

The following options are forwarded to the bridge:

| Option | Default | Description |
|---|---|---|
| `phpstan.level` | `null` | Override the analysis level (0–9). `null` defers to `phpstan.neon`. |
| `phpstan.memoryLimit` | `"512M"` | PHP memory limit for the analyse run. |
| `phpstan.configPath` | `null` | Path to a `phpstan.neon` file. `null` lets PHPStan auto-detect. |
| `phpstan.diagnosticTrigger` | `"onSave"` | When to run analysis. `onChange` is reserved. |
| `phpstan.pinnedVersion` | `null` | Pin a specific PHPStan release tag (e.g. `"1.11.0"`) when downloading the phar. `null` or `"latest"` resolves the latest stable release. |
| `phpstan.showProgress` | `true` | Stream PHPStan's progress bar through `$/progress` reports so Zed shows a live `done / total (%)` indicator. Set to `false` to keep just the spinner. |

## Building Locally

The extension installs the bridge by downloading a pre-built binary from this repository's GitHub releases. For local development you build the bridge yourself and point the extension at it.

### Prerequisites

- Rust toolchain (stable) with the `wasm32-wasip2` target:
  ```bash
  rustup target add wasm32-wasip2
  ```
- PHP 8.0+ on your `PATH` (only required to actually run analysis; the build itself is pure Rust).

### 1. Build the bridge

```bash
cargo build -p phpstan-lsp-bridge --release
```

The binary lands at `target/release/phpstan-lsp-bridge`.

### 2. Make the bridge discoverable to the dev extension

The extension first looks for `phpstan-lsp-bridge` on the worktree's `$PATH` before falling back to its release-download flow. Symlink your freshly built binary into a directory that's on `$PATH` so every reload picks up the latest build:

```bash
mkdir -p ~/.local/bin
ln -sf "$PWD/target/release/phpstan-lsp-bridge" ~/.local/bin/phpstan-lsp-bridge
```

Make sure `~/.local/bin` is on the `$PATH` Zed inherits (on Linux this usually means launching Zed from a shell that already has it set). Rebuilding the bridge updates the symlink target automatically because `cargo build` overwrites the same file.

### 3. Build the WASM extension

```bash
cargo build -p phpstan-zed --target wasm32-wasip2 --release
```

### 4. Install as a dev extension in Zed

In Zed, run **`zed: install dev extension`** from the command palette and pick the `extension/` directory in this repo. Zed compiles the manifest, loads the WASM, and starts the bridge against the symlinked binary.

### Iterating

After changing bridge code:

```bash
cargo build -p phpstan-lsp-bridge --release
```

Then in Zed run **`zed: restart language server`** (or close and re-open the PHP project) so the new binary is picked up.

After changing extension (WASM) code:

```bash
cargo build -p phpstan-zed --target wasm32-wasip2 --release
```

Then in Zed run **`zed: rebuild extension`** (or **`zed: reload extensions`**).

### Useful logs

- **Zed Log** (`zed: open log`) shows extension stdout/stderr and the bridge's `tracing` output.
- For more verbose bridge logs, set `RUST_LOG=debug` in the environment Zed inherits.

## Project Layout

See [AGENTS.md](AGENTS.md) for the full architecture and contribution guide.

## License

MIT — see [LICENSE-MIT](LICENSE-MIT).
