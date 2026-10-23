# Security Policy

## Supported versions

ngx_mzstd is pre-1.0. Security fixes land on `main` and ship in a new patch
release of the latest minor version, as source and as packages on Cloudsmith;
older minor versions are not patched.

| Version   | Supported |
| --------- | --------- |
| 0.2.x     | yes       |
| < 0.2     | no        |

## What counts as a vulnerability

The module runs inside NGINX worker processes and handles requests from
untrusted clients and responses from upstreams. Please report privately
anything that a client request or an upstream response can cause:

- memory unsafety in the worker, such as a use-after-free, an out-of-bounds
  access, a double free or a data race, including on the thread pool path
  (`zstd_thread_pool`);
- a worker crash: release builds abort on panic, so any reachable panic takes
  down the worker and every connection it serves;
- memory or CPU use beyond what the configuration implies, such as encoders,
  buffers or thread-pool tasks that are never released;
- a response that is corrupted, mixed with another request's data, or sent with
  headers that let a shared cache serve compressed content to clients that did
  not accept it.

Problems that need control of `nginx.conf`, of the module binary, or of files
the configuration names (including pre-compressed `.zst` files served by
`zstd_static`) are regular bugs; open an issue for them.

Report elsewhere:

- bugs in the Zstandard encoder itself to
  [mzstd](https://github.com/M-Compressors/mzstd/security/policy);
- bugs in NGINX to the [NGINX project](https://github.com/nginx/nginx/security/policy).

## Reporting a vulnerability

Do not open a public issue, pull request, or discussion.

Report it through GitHub's private vulnerability reporting:
[open a security advisory][advisory]. Include:

- the module version, and whether you built it from source or installed a
  package (and which one);
- the NGINX version and the output of `nginx -V`;
- the relevant part of the configuration;
- a request, and if needed an upstream response, that reproduces the problem,
  plus the `error.log` excerpt or a backtrace from the core dump;
- your assessment of the impact.

## What to expect

- An acknowledgement within 7 days.
- An initial assessment, and a fix plan if the report is confirmed, within
  30 days.
- A coordinated release: the fix is published as source and packages together
  with a GitHub security advisory, and you are credited unless you ask not to
  be.

Please give us a reasonable time to release a fix before disclosing the issue
publicly.

[advisory]: https://github.com/M-Compressors/ngx_mzstd/security/advisories/new
