# Changelog

## [0.2.0](https://github.com/Xevion/ferrite/compare/v0.1.4...v0.2.0) (2026-04-09)


### Features

* Add event bus, RunResults type, and post-run error analysis ([65d0286](https://github.com/Xevion/ferrite/commit/65d0286e99493d89d3b967c96624815a1006b1c5))
* Add HeadlessPrinter and ResultsDoc/ResultsRenderer abstractions ([c58befc](https://github.com/Xevion/ferrite/commit/c58befc11c4b803c72a17309279cc365a7bc0dde))
* Versioned NDJSON schema, reloadable tracing, and Log event ([d5b3eac](https://github.com/Xevion/ferrite/commit/d5b3eac74733d8f7e4dac200b2740f1bdd15e3a8))


### Bug Fixes

* Eliminate 5s exit delay and restore post-TUI tracing ([a81a8ab](https://github.com/Xevion/ferrite/commit/a81a8ab9224fad4cfcf59cb18ada1a41e6bc2859))
* **tempo:** Update tempo config to latest format ([1fc81ef](https://github.com/Xevion/ferrite/commit/1fc81efbb8a9ff2cbe053ddea31a0cf8d8ce0f14))


### Code Refactoring

* Extract EventBridge and rename TuiError to TuiFailure ([47eaff0](https://github.com/Xevion/ferrite/commit/47eaff03a3ea8ba1a275ac8b43fa43e6db6cca3d))
* Replace OutputSink in runner with EventTx; add bridge threads ([38fa60e](https://github.com/Xevion/ferrite/commit/38fa60e556718ccad65d13e5e34829fc4994b54d))
* Replace OutputSink with NdjsonEventWriter + HeadlessPrinter ([38ce935](https://github.com/Xevion/ferrite/commit/38ce935bf7ba46bbd79c046693d25301d11e37a8))

## [0.1.4](https://github.com/Xevion/ferrite/compare/v0.1.3...v0.1.4) (2026-04-06)


### Features

* Add unified shutdown module with signal handling and escalation ([0fae03b](https://github.com/Xevion/ferrite/commit/0fae03b78b6cf6299b0b957e898b02eeaadff204))
* Catch runner panics and surface actionable permission hints ([679ffdc](https://github.com/Xevion/ferrite/commit/679ffdc23b959d63c076365fd390f793c239ed90))


### Code Refactoring

* Extract run_event_loop&lt;B&gt; and add event loop test suite ([7c0e346](https://github.com/Xevion/ferrite/commit/7c0e346a7594818659cc54a9abc3505da303eb02))
* Remove indicatif now that ratatui covers progress display ([26d98b7](https://github.com/Xevion/ferrite/commit/26d98b7049223c8806710aa19264eff07c30b1a0))
* Rename LockedRegion to TestBuffer, RegionState to Segment; fix error to failure vocabulary ([705f3c3](https://github.com/Xevion/ferrite/commit/705f3c3c65b4b2be5f97594036819bbec22f4c3a))


### Miscellaneous

* Drop MSRV badge and rust-version constraint ([1755b3e](https://github.com/Xevion/ferrite/commit/1755b3e1f337c3696e7dacb05f19973864bc441c))
* Pin rust toolchain, CI tool versions, and add Justfile aliases ([cb52c10](https://github.com/Xevion/ferrite/commit/cb52c10951d00981fcff1831c419379393cea08c))

## [0.1.3](https://github.com/Xevion/ferrite/compare/v0.1.2...v0.1.3) (2026-04-05)


### Code Refactoring

* Broken-pipe handling, truncated failures, nextest tracing ([4c12d60](https://github.com/Xevion/ferrite/commit/4c12d6006a6cc79451cdd8978c28cd2e960c3245))
* Consolidate SMBIOS parsing into focused helper functions ([25e6be0](https://github.com/Xevion/ferrite/commit/25e6be08a69992b68c10d47f85bd2f507c9da2c3))
* Extract privilege checks and scalar ops into testable units ([f93f867](https://github.com/Xevion/ferrite/commit/f93f8674b44b5360c3751545f0288cd85de3b7de))
* Reorganize simd.rs and scalar ops into ops/ module ([e9ff3d1](https://github.com/Xevion/ferrite/commit/e9ff3d1dbd60da1a565e0571e669d5375ee5039e))


### Miscellaneous

* Configure nextest CI profile, codecov components, ignore simd.rs coverage ([40d13a6](https://github.com/Xevion/ferrite/commit/40d13a686183c675ace321f98c80be64d926a441))
* Configure nightly coverage attributes and suppress TUI noise ([a0aa8df](https://github.com/Xevion/ferrite/commit/a0aa8dff617cc712455361e955aa74b80197fe12))
* Wire up samply, perf, and cargo-mutants for dev analysis ([708223f](https://github.com/Xevion/ferrite/commit/708223f20ace80692223bd61c86dc9553f983ac6))

## [0.1.2](https://github.com/Xevion/ferrite/compare/v0.1.1...v0.1.2) (2026-04-05)


### Features

* Add --tui auto|always|never flag and unify mode dispatch ([f000221](https://github.com/Xevion/ferrite/commit/f000221d745944abd319408d9f9ed8a9162d945f))
* Add physical address resolution, ECC monitoring, and DIMM topology ([302a431](https://github.com/Xevion/ferrite/commit/302a43137c3db024df0f6eb0425025b51cf444ca))
* Add ratatui TUI with activity heatmaps and multi-region parallel testing ([ee7741f](https://github.com/Xevion/ferrite/commit/ee7741f45effdf992f2a6aa06d0db63f1fcc293e))


### Bug Fixes

* Add anyhow context to TUI terminal errors for clearer diagnostics ([2a9646c](https://github.com/Xevion/ferrite/commit/2a9646c46726c770eea43cc512ed356a8e4efd9d))
* Prevent parse_size overflow and add property-based test coverage ([6869865](https://github.com/Xevion/ferrite/commit/686986503c1acede5796f087a2cb6f8d69679d49))


### Code Refactoring

* Add format_size as parse_size inverse and broaden proptests ([809b947](https://github.com/Xevion/ferrite/commit/809b9470c1e6d95b5d869138a17cb04a276f0ceb))
* Break up main.rs into cli, failure, pattern, and tui modules ([b7417ce](https://github.com/Xevion/ferrite/commit/b7417ce66326553a10163d137a26c7686816f8bb))
* Cfg-gate TUI logic and expand CI to test all feature combos ([acf8372](https://github.com/Xevion/ferrite/commit/acf8372b2916c560ac3381cfb240fd3855a8c09a))
* Enforce clippy pedantic lints and modernize pointer casts ([b90fccd](https://github.com/Xevion/ferrite/commit/b90fccd642554116def05232dd1ffd91ba1f54e6))
* Fix len_without_is_empty lint and verify Pattern::ALL scope ([1fab629](https://github.com/Xevion/ferrite/commit/1fab629fcf4f7c72f04ad2ae3a90b89c16789fe3))
* Migrate tests to assert2/rstest with FailureBuilder fixture ([a0997fd](https://github.com/Xevion/ferrite/commit/a0997fdaa2b5ff0981d8c59bc1e4ac705d3e6c97))
* Name spawned threads, promote tracing dependency ([25069c3](https://github.com/Xevion/ferrite/commit/25069c3435aa20b098e0c711d95c82475df5197f))
* Unify tracing and output plumbing across run modes ([fe394fa](https://github.com/Xevion/ferrite/commit/fe394fac391ba5d913a9833e968b3ccb7f987b74))


### Documentation

* Correct MSRV badge and requirement from 1.85 to 1.89 ([1117029](https://github.com/Xevion/ferrite/commit/11170292c9d89170493610b658a43a539cba60c4))


### Miscellaneous

* **deps:** Bump codecov/codecov-action from 5 to 6 ([9945794](https://github.com/Xevion/ferrite/commit/994579412b323069dc59f4b01321b73024c39096))
* Integrate tempo as task runner for checks, lint, and format ([b18e28f](https://github.com/Xevion/ferrite/commit/b18e28fcea009fc7995f180d314b43b9ed855787))

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
* Rework release workflow to use release event instead of tag push ([4a2cf08](https://github.com/Xevion/ferrite/commit/4a2cf08b46a7a1d9c7ee86146881b25650662ad6))


### Miscellaneous

* **deps:** Bump actions/checkout from 5 to 6 ([e5aa1d2](https://github.com/Xevion/ferrite/commit/e5aa1d20e3562db90d5348804f4494704ce6426b))
* **deps:** Bump actions/upload-artifact from 4 to 7 ([020fbb8](https://github.com/Xevion/ferrite/commit/020fbb800e44810bf745b47755623c26681e699b))
