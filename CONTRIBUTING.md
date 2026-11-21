# Contributing to `duke-link`

Thank you for contributing to `duke-link`! Please follow these guidelines so pull requests merge smoothly.

## Code Quality & Safety Standards

- **Zero Panics in Production Rust:** Every crate in the workspace inherits `[lints] workspace = true` with `clippy::unwrap_used = "deny"` and `clippy::expect_used = "deny"` (`clippy.toml` permits `unwrap`/`expect` inside `#[test]` functions only). Always propagate typed `thiserror` errors with `?`.
- **Current Stable `iroh` API:** Use `Endpoint::builder`, `EndpointId`, and `EndpointAddr` from `iroh = "1.5.0"`.
- **Filesystem & Network Hygiene:** Canonicalize and prefix-check all filesystem paths derived from remote input. Never hardcode personal IPs, hostnames, API keys, or Ed25519 private keys.
- **File Encoding:** Save all source, config, and script files as UTF-8 without BOM.

## Local Verification Before Opening a PR

Run the full hygiene suite and frontend syntax check:

```powershell
node --check crates\client-app\src\main.js
.\scripts\verify-hygiene.ps1
```

On Linux/macOS, run the equivalent commands:

```bash
node --check crates/client-app/src/main.js
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Pull Request Checklist

- [ ] `cargo fmt --all --check` passes cleanly.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes with zero warnings.
- [ ] `cargo test --workspace` (including `shared-protocol` tests, `client_unit_tests`, and `quic_loopback_test`) passes.
- [ ] `node --check crates/client-app/src/main.js` passes.
- [ ] No secrets, personal paths, or private keys are included in the diff.
