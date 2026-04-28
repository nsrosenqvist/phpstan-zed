# AGENTS.md — Working on `phpstan-zed`

> **Read this first.** It is the orientation document for any human or AI
> agent that picks up work on this repository.

---

## North Star

**Bring real-time PHPStan diagnostics into Zed without coupling to any other
PHP tooling.**

Every change must protect that promise. If a change adds dependencies,
introduces awareness of other PHP language servers, or moves us away from a
self-contained Rust-only bridge, it does not belong here.

### Value Proposition (must be preserved)

- Open a PHP file in Zed → PHPStan errors render inline on save with **zero
  configuration** for projects that ship PHPStan via Composer.
- Open a PHP project that does **not** ship PHPStan → the extension downloads
  a phar from PHPStan's GitHub releases and works anyway.
- The user installs **one** Zed extension. They never install Composer
  packages, Node.js tooling, or third-party PHP language-server bridges.

### Hard Constraints

1. **No PHP dependency tree.** The bridge is pure Rust; PHP is required only
   as the runtime for PHPStan itself. We never run `composer install`.
2. **No coupling.** This extension is independent of every other PHP
   extension in the Zed marketplace. Diagnostics may overlap with other tools
   — that is acceptable and expected.
3. **No bundled binaries inside the extension.** Both the bridge and PHPStan
   are downloaded at runtime from GitHub releases. The WASM extension stays
   small.
4. **Strict typing, no magic strings.** Settings are enums, paths are
   `PathBuf`, errors are exhaustive enums. If you find yourself comparing a
   `String` to a literal, model it as a type.
5. **All fallible operations return typed errors.** In the bridge, that is
   `BridgeError`. In the extension, that is `zed_extension_api::Result<T>`,
   which uses `String` — wrap context explicitly.

### Non-Goals

- Code completion, go-to-definition, refactoring — anything that is not a
  PHPStan diagnostic.
- Bundling PHP or PHPStan itself.
- Awareness of `intelephense`, `phpactor`, or other PHP language servers.

---

## Repository Map

The workspace has two crates plus top-level metadata. File-level structure
inside each crate evolves freely; the **directory layout** and **domain
ownership** below are what stays stable.

```
phpstan-zed/
├── AGENTS.md, README.md, PLAN.md, LICENSE-MIT   ← top-level docs
├── Cargo.toml                                   ← workspace root
├── extension/                                   ← Zed WASM extension
│   ├── extension.toml                           ← Zed manifest (id, version, grammars)
│   ├── Cargo.toml                               ← cdylib, target = wasm32-wasip2
│   └── src/                                     ← Rust sources (no I/O beyond Zed API)
└── bridge/                                      ← Native LSP server
    ├── Cargo.toml                               ← binary + library
    ├── src/                                     ← Rust sources (tokio, tower-lsp)
    └── tests/                                   ← integration tests (in-process LSP, opt-in real PHPStan)
```

### Domain Ownership (do not violate)

Boundaries are described by **domain**, not by individual files. When a new
concern shows up, place it in the domain that owns it; if a file would
straddle two domains, split it.

| Domain | Lives in | Owns | Does NOT own |
|---|---|---|---|
| Extension wiring | `extension/src/` (entry point) | `Extension` trait impl, registration, glue between domains below | Process spawning, LSP message handling |
| Bridge lifecycle | `extension/src/` | Bridge binary discovery, download, cache, `zed::Command` construction | PHPStan resolution, LSP types |
| PHPStan resolution | `extension/src/` | Locating or downloading the PHPStan binary, version pinning, on-disk cache | Anything bridge-related |
| User-facing settings | `extension/src/` | Strongly-typed init options, JSON shape, defaults | I/O, downloads, process work |
| LSP server | `bridge/src/` | `tower-lsp` glue, capability declaration, lifecycle, progress plumbing | PHPStan invocation, JSON-to-LSP mapping |
| PHPStan invocation | `bridge/src/` | `PhpStanRunner` trait + CLI implementation, stderr/stdout handling, progress parsing | LSP types, diagnostic mapping |
| Diagnostic mapping | `bridge/src/` | Pure functions from PHPStan JSON to LSP `Diagnostic` | Process spawning, transport |
| Cross-cutting types | `bridge/src/` | Config, errors, shared enums | Behaviour |

Rules of thumb:

- The extension never speaks LSP; the bridge never speaks Zed extension API.
- The bridge never prints to stdout (that is the LSP transport).
- Subprocess work happens behind the `PhpStanRunner` trait so tests can fake
  it.
- Anything user-visible (settings, CLI flags, diagnostic shape) gets a unit
  test that pins the JSON / argv shape.

---

## Architecture in 60 Seconds

```
PHP file save in Zed
    │
    ▼
Zed sends `textDocument/didSave` over stdio to phpstan-lsp-bridge
    │
    ▼
PhpStanLspServer::did_save → PhpStanRunner::analyse
    │
    ▼
CliPhpStanRunner spawns: php <phpstan> analyse --error-format=json --no-progress <file>
    │
    ▼
PhpStanOutput::from_json parses stdout
    │
    ▼
diagnostics::map_diagnostics produces Vec<Diagnostic>
    │
    ▼
client.publish_diagnostics(uri, diagnostics, None)
    │
    ▼
Zed renders inline.
```

The extension itself only resolves binaries and assembles a `zed::Command`.
It does not touch LSP messages.

### PHPStan Resolution Tiers (in order)

1. `<worktree>/vendor/bin/phpstan` (Composer-installed, project-correct).
2. `phpstan` on `$PATH`.
3. Pinned-version phar download (reserved for future settings).
4. Latest stable phar download from `phpstan/phpstan` GitHub releases.

Once any tier succeeds the path is cached on the resolver instance for the
session.

### Bridge Distribution

Bridge binaries are built per-platform and uploaded as release assets to
`nsrosenqvist/phpstan-zed`. Asset naming convention:

```
phpstan-lsp-bridge-{arch}-{os_triple}{.tar.gz|.zip}
```

Driven by the bridge-lifecycle domain's `bridge_asset_name` helper.
**Update the CI matrix in lockstep with that function.**

---

## Conventions

### Style

- Edition **2024**, Rust 2024 idioms, `#![deny(unsafe_code)]` is implicit
  (we never need `unsafe`).
- Public items get rustdoc with at least one sentence stating the purpose
  and any non-obvious invariants.
- No `unwrap()`/`expect()` outside tests. Bridge errors flow through
  `BridgeResult<T>`; extension errors through the API's `Result<T, String>`
  with explicit `format!` context.
- `serde` derives must opt in to `#[serde(rename_all = "camelCase")]` for
  any JSON exchanged with editors.
- Strings that appear in JSON or CLI flags live as `pub const` next to
  their consumer or as enum variants. **No magic strings.**

### Adding a new setting

1. Add a typed field to the user-facing settings type with a sensible
   `Default`.
2. Add a unit test verifying the JSON serialization shape.
3. Thread the value into the bridge via `BridgeConfig` and a matching CLI
   flag in the bridge entry point.
4. Update `README.md` with the user-facing description.

### Adding a new resolution tier

1. Extend `PhpStanResolver::resolve` in the correct order — earlier tiers
   must not regress.
2. Cover the new tier with a unit test that operates on path predicates
   only (no real I/O).
3. Document the precedence change in `README.md`.

### Logging

- The bridge's stdout is the LSP transport. **Never** print to stdout.
- Use `tracing::{info,debug,error}` macros. The default `EnvFilter` is
  `info`; set `RUST_LOG=debug` to enable verbose output during debugging.

---

## Common Tasks

| Task | Command |
|---|---|
| Build the bridge | `cargo build -p phpstan-lsp-bridge --release` |
| Build the extension | `cargo build -p phpstan-zed --target wasm32-wasip2 --release` |
| Run all tests | `cargo test --workspace` |
| Run extension tests only (host) | `cargo test -p phpstan-zed` |
| Lint everything | `cargo clippy --workspace --all-targets -- -D warnings` |
| Format | `cargo fmt --all` |
| Install dev extension in Zed | Zed: `zed: install dev extension` and pick `extension/` |

The wasm target must be installed once: `rustup target add wasm32-wasip2`.

---

## Testing Strategy

- **Unit tests** sit next to the code in `#[cfg(test)] mod tests`. Pure
  logic only; no real I/O.
- **Integration tests** live in `bridge/tests/`. They exercise the LSP server
  in-process via `tokio::io::duplex` against a fake `PhpStanRunner`.
- **Manual smoke test** is required for any user-visible change: see
  *Verification Checklist* below.

When adding a test:

- Prefer pure functions and trait fakes over real subprocesses.
- For new PHPStan output shapes, paste a real JSON sample into the
  PHPStan-invocation domain's tests so the schema is locked.
- For new LSP behaviour, extend the in-process LSP integration tests under
  `bridge/tests/` with a fresh `#[tokio::test]`.

---

## Verification Checklist (a task is "done" only when all items pass)

A task touching code is **not** complete until every relevant item below is
green. Tick them off in order.

### Always

- [ ] `cargo fmt --all -- --check` is clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] `cargo test --workspace` passes (host tests).
- [ ] `cargo build -p phpstan-zed --target wasm32-wasip2 --release`
      succeeds with no warnings.
- [ ] `cargo build -p phpstan-lsp-bridge --release` succeeds.
- [ ] No new `unwrap()`/`expect()` introduced outside `#[cfg(test)]`.
- [ ] No new magic strings — every literal that appears in a CLI flag, env
      var, JSON key, or asset name is a named constant or enum variant.
- [ ] Public items added/changed have rustdoc.

### When changing the bridge

- [ ] New behaviour has a `#[cfg(test)]` unit test next to the code, or an
      integration test under `bridge/tests/` using a fake `PhpStanRunner`.
- [ ] All tests still pass: `cargo test -p phpstan-lsp-bridge`.

### When changing the extension

- [ ] Settings changes have a JSON-shape unit test alongside the settings
      domain.
- [ ] Resolver changes have a path-predicate unit test alongside the
      PHPStan-resolution domain.
- [ ] Asset naming changes are mirrored in the CI release matrix.

### When changing user-visible behaviour

- [ ] `README.md` is updated.
- [ ] `extension/extension.toml` `version` is bumped (semver).
- [ ] Manual smoke test:
  1. `cd extension && cargo build --target wasm32-wasip2 --release`
  2. In Zed: *zed: install dev extension* → pick `extension/`.
  3. Open a PHP project with `vendor/bin/phpstan` → diagnostics appear on
     save.
  4. Open a PHP project without PHPStan → confirm download progress in Zed
     and that diagnostics appear afterwards.
  5. Temporarily remove `php` from `$PATH` → confirm the user-facing error
     message is the one produced by the bridge-lifecycle domain when PHP
     cannot be resolved.

If you cannot run the manual smoke test (e.g. no Zed available), say so
explicitly in the PR description.

---

## Anti-Patterns to Avoid

- **Reaching into internals from the extension entry point.** Always go
  through the helper structs that own each domain (bridge lifecycle,
  PHPStan resolution, settings).
- **Embedding strings in `format!` calls.** Lift them to `const`.
- **Adding new dependencies casually.** Each new crate widens the supply
  chain. Justify every addition in the PR.
- **Spawning subprocesses outside the PHPStan-invocation domain.** All
  process work goes through the `PhpStanRunner` trait so tests can fake it.
- **Mutating cached resolver state without verifying the path still
  exists.** A user can delete the cache between sessions.

---

## Pointers for New Contributors

If you are picking this up and want to add something:

- **A new diagnostic field** (e.g. severity inference): start in the
  diagnostic-mapping domain. Add a unit test next to the change.
- **A new PHPStan flag** (e.g. `--memory-limit`): pipe it through
  `BridgeConfig` and `CliPhpStanRunner::build_command`. Snapshot the args
  in the existing build-command tests.
- **Pin a PHPStan version**: read it out of the user-facing settings type
  and forward it to `PhpStanResolver::with_pinned_version` (already wired).
- **A new platform**: extend `bridge_asset_name` and the CI matrix.

When in doubt, re-read the **Hard Constraints** above. They are the brakes.
