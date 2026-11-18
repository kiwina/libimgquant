# Security

Report vulnerabilities privately, with **Report a vulnerability** on this repository's
[Security tab](https://github.com/kiwina/libimgquant/security) (a GitHub security advisory),
not in a public issue. Fixes go into the latest 3.x release.

In scope:

- **PNG decoding** of untrusted files: the library's `png` feature, the `imgquant`
  command-line tool, and the npm and Python packages.
- **The C API:** the unsafe `extern "C"` code in `libimgquant-capi` (`capi/`).
