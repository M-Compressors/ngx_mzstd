## Summary

<!-- What does this change and why? Link the issue it addresses (Fixes #...). -->

## Compatibility

<!-- Does this change behaviour compared with tokers/zstd-nginx-module or with the
     previous release (directives, defaults, headers, responses)? If so, how
     is it documented? Otherwise write "None". -->

## Performance

<!-- For performance-sensitive changes: tool, workload, machine, and
     before/after numbers. Otherwise write "N/A". -->

## Checklist

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets -- -D warnings` (with and without `--no-default-features`)
- [ ] `cargo test`
- [ ] Integration tests in `t/` (`prove -I nginx-tests/lib t/`)
- [ ] `cargo +nightly miri test`, if the body filter changed
- [ ] Bug fixes include a test that fails without the fix
- [ ] README updated for new or changed directives, variables, and differences from tokers/zstd-nginx-module
- [ ] Sampo changeset added (`sampo add`) for user-facing changes
