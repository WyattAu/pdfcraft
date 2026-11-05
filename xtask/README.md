# Repository checks

Run `cargo xtask` for the available commands. The vendor ledger check is:

```sh
cargo test --locked -p xtask vendor_ledger
cargo run --locked -p xtask -- vendor-ledger
```

Add `--list` after `vendor-ledger` to print each citation's classification.

The checker compares the versions and patch entries in `vendor/README.md`
with the patched crate manifests and source markers. Every ledger row has
five columns: crate, version, licence, patches, and regression tests. Keep
test citations in their own column when consolidating duplicate rows.

Citation names are indexed across `vendor/`, `apps/pdfcraft/`, and `crates/`.
A file-only citation such as `lib.rs` requires an indexed file. A qualified
citation such as `lib.rs::tests::replacement` also requires its final symbol.
This is a lightweight name index, not a Rust parser: it does not establish
the full module path, require a `#[test]` attribute, or prove that a cited
test ran. Bare snake_case names in the Test column must resolve to a function or module.

The scanner reads ASCII identifiers at the keyword boundary, ignores function
pointer syntax, and checks the immediate function parameter list, allowing
up to 64 characters of balanced generics. It tolerates Unicode surrounding
text without slicing inside a character. Patch-marker warnings use the
highest explicit `(n)` number from 1 through 500, including numbers after a
gap; prose such as `(4 GiB)` is not a patch number. Gaps are not filled in.

The ordinary workspace test suite runs the checker's regression tests. Run
`cargo xtask vendor-ledger` explicitly to audit the current ledger. To test
a vendored crate itself, use, for example,
`cargo test --manifest-path vendor/hayro-syntax/Cargo.toml --lib`.
