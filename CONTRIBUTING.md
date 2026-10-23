# Contributing to ngx_mzstd

Thank you for helping. Bug reports, compatibility reports, documentation fixes,
and code are all welcome. By participating you agree to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

Security issues are reported privately; see [SECURITY.md](SECURITY.md).

## Before you start

- For anything beyond a small fix, open an issue first so we can agree on the
  approach before you invest time in it.
- ngx_mzstd is a drop-in replacement for [tokers/zstd-nginx-module](https://github.com/tokers/zstd-nginx-module):
  the same modules, directives, defaults and variable. A change that breaks an
  existing configuration, or that departs from tokers/zstd-nginx-module's behaviour,
  needs a reason and an entry in
  [Differences from tokers/zstd-nginx-module](README.md#differences-from-tokerszstd-nginx-module).
- Compression itself lives in [mzstd](https://github.com/M-Compressors/mzstd).
  Changes to the bytes the encoder writes belong there; this repository drives
  the encoder from NGINX's filter chain.
- Access C bit-fields only through `src/bitfields.c`. The accessors bindgen
  generates do not match the C compiler's layout; see "Implementation notes"
  in the README.

## Setting up

You need Rust 1.89 or newer, a C compiler, libclang (for `nginx-sys`), the
`zstd` command-line tool, and an NGINX source tree. Build NGINX the way CI
does, with threads so that `zstd_thread_pool` is compiled in:

```sh
git clone https://github.com/M-Compressors/ngx_mzstd.git
curl -sSfL https://nginx.org/download/nginx-1.30.5.tar.gz | tar xz
cd nginx-1.30.5
./configure --with-compat --with-debug --with-http_ssl_module --with-threads
make -j
export NGINX_BUILD_DIR=$PWD/objs
cd ../ngx_mzstd
```

`./configure` is enough for building the module and running the unit tests;
`make` produces the `nginx` binary that the integration tests run.

## Checks to run before opening a pull request

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test
cargo build --release
git clone https://github.com/nginx/nginx-tests
TEST_NGINX_BINARY=$NGINX_BUILD_DIR/nginx \
TEST_NGINX_GLOBALS="load_module $PWD/target/release/libngx_mzstd.so;" \
    prove -I nginx-tests/lib t/
```

Use `libngx_mzstd.dylib` on macOS. `zstd_thread_pool` is compiled
only for an NGINX built with threads, so after touching code behind that
condition also run Clippy against a tree configured without `--with-threads`.

After changing the body filter (`src/body.rs` and `src/body/`), also run it
under Miri, which checks it against a model of NGINX's buffer and chain API:

```sh
rustup +nightly component add miri rust-src
cargo +nightly miri test
```

CI also runs the integration tests against NGINX 1.28, 1.30 and 1.31, and
under AddressSanitizer and LeakSanitizer; see "Contributing and releases" in the
README for what each job covers.

## Tests

- Unit tests sit beside the code they test; tests that need a running NGINX go
  in `t/` and use the [nginx-tests](https://github.com/nginx/nginx-tests)
  framework.
- Fixing a bug starts with a test that fails without the fix.
- New directives and changed defaults need tests for parsing, inheritance
  between `http`, `server` and `location`, and the resulting responses.

## Documentation and changelog

- Document new or changed directives and variables in the README, in the same
  form as the existing ones (syntax, default, context, behaviour), and update
  the sample configuration if it is affected.
- Every user-facing change needs a [Sampo](https://github.com/bruits/sampo)
  changeset in `.sampo/changesets/`; do not edit `CHANGELOG.md` or the version
  in `Cargo.toml` by hand:

  ```sh
  cargo install sampo
  sampo add
  ```

## Pull requests

- Keep each pull request focused on one change.
- Fill in the pull request template, including which checks you ran.
- CI must pass before merging. Releases are cut from the **Release (main)**
  pull request that Sampo opens; packages are published by a maintainer.

## License

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you shall be licensed under the
[MIT license](LICENSE), without any additional terms or conditions.
