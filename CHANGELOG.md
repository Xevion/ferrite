# Changelog

## [0.1.1](https://github.com/Xevion/ferrite/compare/v0.1.0...v0.1.1) (2026-03-20)


### Features

* Add GPL-3.0 license, MSRV rust-version, and CI/README badges ([deb58c7](https://github.com/Xevion/ferrite/commit/deb58c7dcf4e004aa1f10909911cb91bdb92bab2))
* Display per-pattern throughput and support binary/decimal units ([b0bb3e5](https://github.com/Xevion/ferrite/commit/b0bb3e51edde10643d39213ca04eecb73364b5ec))
* Initial ferrite implementation ([085a1a8](https://github.com/Xevion/ferrite/commit/085a1a87fcdf63ddb04d869a08dfe5dafbac594c))
* Support NDJSON event streaming for scripted consumption ([4b05236](https://github.com/Xevion/ferrite/commit/4b0523661eb7f3cc7f164ba1b15b89114c80245d))
* Warn on missing/insufficient root/CAP_IPC_LOCK before attempting mlock ([2c8b229](https://github.com/Xevion/ferrite/commit/2c8b229fcaf03e0feb70c51db8241e30a027e9f4))


### Performance Improvements

* Accelerate fills with AVX-512 NT stores and parallel page faulting ([34f4386](https://github.com/Xevion/ferrite/commit/34f438644c03b4c15849da54ce5d763462172c5e))


### Code Refactoring

* Move AVX-512 intrinsics to simd module and Failure to lib root ([6b740f5](https://github.com/Xevion/ferrite/commit/6b740f5250870d32915c8bf9674091d3c0500b80))


### Continuous Integration

* Add GitHub Actions workflows, dependabot, and cargo-deny config ([94e1acf](https://github.com/Xevion/ferrite/commit/94e1acfdc38c00391f5180027a8fdfc1e7d4ed61))
* Add release-please automation and update README to reflect current state ([6db26a8](https://github.com/Xevion/ferrite/commit/6db26a85548686490c71530f3cf21180e5795698))


### Miscellaneous

* **deps:** Bump actions/checkout from 5 to 6 ([e5aa1d2](https://github.com/Xevion/ferrite/commit/e5aa1d20e3562db90d5348804f4494704ce6426b))
* **deps:** Bump actions/upload-artifact from 4 to 7 ([020fbb8](https://github.com/Xevion/ferrite/commit/020fbb800e44810bf745b47755623c26681e699b))
