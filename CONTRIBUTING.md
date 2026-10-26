# Rust development

The workspace targets Rust 1.99 on stable. `rust-toolchain.toml`, the inherited
`rust-version`, CI, and the Nix development shell use that baseline. After changing
the flake, run `direnv reload`.

## Validation

Run these commands in the development shell:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo check --locked -p meowland-core --no-default-features
cargo check --locked -p meowland-protocol --no-default-features
cargo test --locked --workspace --all-features
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --all-features --no-deps
cargo audit --deny warnings
cargo deny --locked check
cargo machete --with-metadata
```

Follow `AGENTS.md` before adding tests or changing repository documentation.

## Applying BeautifulRust

The workspace follows the applicable guidance from
[BeautifulRust](https://github.com/morganlinton/BeautifulRust): validated data at
boundaries, explicit state transitions, narrow visibility, typed library errors,
documented public APIs and unsafe contracts, and reasoned local lint exceptions.
These are unpublished application components rather than independently versioned
public libraries; their interfaces should stay small and cohesive.

`all`, `pedantic`, and `nursery` Clippy groups are enabled alongside targeted
correctness and API lints. `pub(crate)` deliberately marks cross-module internals,
even in private modules; `unreachable_pub` guards against accidental public APIs.
The application-oriented naming and `must_use_candidate` exceptions avoid
mechanical naming changes and annotations without an actionable contract.

Use checked conversions at protocol and allocation boundaries. When rounding,
truncation, or approximate telemetry is intentional, document the invariant in a
local `#[expect(..., reason = "...")]`. An `expect` must describe an internal
invariant, not replace handling of malformed client input or ordinary I/O errors.

Keep blocking initialization and thread joins off async workers. Prefer explicit
async shutdown that reports failures; destructors should only signal cancellation
and release resources, not join threads or acquire potentially poisoned locks.
Shared-buffer ownership should preserve allocation reuse: `Arc<Vec<u8>>` is
intentional where frame allocations move into the presenter and are reused with
copy-on-write.

Dependencies are centralized in the workspace. Git dependencies are pinned to
revisions, and the committed lockfile is used in CI. `deny.toml` enforces license,
advisory, and source policies. Incompatible transitive versions required by
upstream crates remain visible as duplicate-version warnings rather than being
forced together with patches.
