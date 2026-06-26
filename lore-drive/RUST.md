# HANDOFF: Rust Toolchain Setup in Claude's Sandbox

If you're not Claude, what follows may or may not apply, then if anything special,
append a new section for "you".

## Context

Claude's sandbox runs **Ubuntu 24.04 Noble**. The default `rustc`/`cargo` from APT
(`noble/main`) is pinned to **1.75.0**, which predates edition 2024 (requires ≥ 1.85).

However, `noble-updates/universe` ships versioned Rust snapshot packages up to
**1.91.1** (as of June 2026). No rustup, no network tricks needed — pure APT.

### What doesn't work (already investigated)
- `rustup.rs` → blocked by egress proxy (`host_not_allowed`)
- GitHub release asset downloads → redirect to `release-assets.githubusercontent.com` → blocked
- `static.rust-lang.org` → blocked
- `objects.githubusercontent.com` → blocked

---

## Toolchain Setup (copy-paste, run as root / via bash_tool)

```bash
# 1. Install Rust 1.91 (edition 2024 support, latest available as of 2026-06)
apt-get install -y rust-1.91-all

# 2. Wire up rustc and cargo via update-alternatives
update-alternatives --install /usr/bin/rustc  rustc  /usr/bin/rustc-1.91  100
update-alternatives --install /usr/bin/cargo  cargo  /usr/bin/cargo-1.91  100

# 3. Verify
rustc --version   # → rustc 1.91.1 (ed61e7d7e 2025-11-07)
cargo --version   # → cargo 1.91.1
```

> **Note:** `rust-toolchain.toml` is informational only here — apt-installed cargo
> has no rustup underneath, so it cannot switch channels. The file is still useful
> for editors (rust-analyzer) and documents the intended toolchain.

### Available versioned packages (apt)

| APT package      | rustc version | Edition 2024 |
|------------------|---------------|:------------:|
| `rust-1.74-all`  | 1.74.x        | ❌           |
| `rust-1.75-all`* | 1.75.0        | ❌           |
| `rust-1.85-all`  | 1.85.1        | ✅ minimum   |
| `rust-1.89-all`  | 1.89.0        | ✅           |
| `rust-1.91-all`  | 1.91.1        | ✅ latest    |

\* The default `rustc` meta-package resolves to 1.75.0.

To discover new versions: `apt-cache search "^rust-1\."` (note: `apt-cache search rustc`
only shows the 1.75 meta-package — the versioned ones are in the `universe` component).

---

## Dummy Project Layout

```
my-project/
├── rust-toolchain.toml
├── Cargo.toml
└── src/
    └── main.rs
```

### `rust-toolchain.toml`

```toml
[toolchain]
channel = "1.91"
```

### `Cargo.toml`

```toml
[package]
name = "my-project"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"   # MSRV: minimum that supports edition 2024

[dependencies]
```

### `src/main.rs`

```rust
fn main() {
    let words = vec!["hello", "sandbox", "edition2024"];
    let upper: Vec<_> = words.iter().map(|w| w.to_uppercase()).collect();
    println!("{}", upper.join(" · "));
    println!("MSRV: {}", env!("CARGO_PKG_RUST_VERSION"));
}
```

### Build & run

```bash
cargo build
cargo run
# or explicitly:
cargo-1.91 run
```

---

## Quick-start one-liner (setup + scaffold + run)

```bash
apt-get install -y rust-1.91-all && \
update-alternatives --install /usr/bin/rustc rustc /usr/bin/rustc-1.91 100 && \
update-alternatives --install /usr/bin/cargo cargo /usr/bin/cargo-1.91 100 && \
cargo new hello-2024 && \
sed -i 's/edition = "2021"/edition = "2024"/' hello-2024/Cargo.toml && \
echo 'rust-version = "1.85"' >> hello-2024/Cargo.toml && \
cd hello-2024 && cargo run
```

---

## Notes for future sessions

- Sandbox resets between sessions — rerun the apt setup each time.
- `cargo-1.91` and `rustc-1.91` binaries are always available after install, even
  without `update-alternatives`, if you want to be explicit.
- Clippy is included in `rust-1.91-all` as `/usr/bin/clippy-driver-1.91`. Wire it
  up with: `update-alternatives --install /usr/bin/clippy-driver clippy-driver /usr/bin/clippy-driver-1.91 100`
- `rustdoc-1.91` is also present if you need docs generation.
- `crates.io` is whitelisted — `cargo add <crate>` and `cargo build` pulling deps
  from the registry works fine.
