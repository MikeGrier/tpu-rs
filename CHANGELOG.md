# Changelog

## [0.5.1](https://github.com/MikeGrier/tpu-rs/compare/v0.5.0...v0.5.1) (2026-06-25)


### Bug Fixes

* correct replace --line-ending on UTF-16 and unify atomic writes ([38cb035](https://github.com/MikeGrier/tpu-rs/commit/38cb035de623e22fada3cd376b924a56a7fb38ee))
* step replace_u16_pairs by whole UTF-16 code units ([c801d57](https://github.com/MikeGrier/tpu-rs/commit/c801d570d247997f74e7f03ed4e9a545d17a93f0))

## [0.5.0](https://github.com/MikeGrier/tpu-rs/compare/v0.4.0...v0.5.0) (2026-06-25)


### Features

* **doctor:** git-aware line-ending detection and normalization ([d8d97a1](https://github.com/MikeGrier/tpu-rs/commit/d8d97a19d0b4094e4624b350928a9ad637c6bb87))


### Bug Fixes

* **ci:** install cross-compile target for pinned toolchain (1.95.0) not stable ([bab52be](https://github.com/MikeGrier/tpu-rs/commit/bab52be8cb4cf1e9275ed608eea898bfbb8a7234))
* **doctor:** report encoding-invalid files even with allow-mojibake marker ([1f68e2d](https://github.com/MikeGrier/tpu-rs/commit/1f68e2d5e179b2a97bff6287012399a92409d16a))
* **eol:** gate EOL note to line mode, correct setting name, fix UTF-16 docstring ([3da7840](https://github.com/MikeGrier/tpu-rs/commit/3da784001b39ba837d95fed66b0ca0447e922cb1))
* **git:** report non-conforming EOL as actual; accept file:// git_root in MCP ([e27f883](https://github.com/MikeGrier/tpu-rs/commit/e27f8834128ed6e808018c77550cbb459d81e098))

## [0.4.0](https://github.com/MikeGrier/tpu-rs/compare/v0.3.1...v0.4.0) (2026-06-21)


### Features

* **doctor:** detect U+FFFD replacement-character residue ([e5f20c8](https://github.com/MikeGrier/tpu-rs/commit/e5f20c8ade5d03982e61b5c927dc67b06f4a47d0))
* **tpu-mcp:** switch tool output to NDJSON ([50b496c](https://github.com/MikeGrier/tpu-rs/commit/50b496c653cb7a1b13b1456202c0aa2a3f80604e))


### Bug Fixes

* address Copilot PR review comments ([f5f5d61](https://github.com/MikeGrier/tpu-rs/commit/f5f5d611fb90ed3dc8fcf8069eb6c893ffb4e057))
* collapse multi-line string literal to explicit \\n escape ([e870b9f](https://github.com/MikeGrier/tpu-rs/commit/e870b9f3afb34e70f68687731a64fad8e22a205f))
* **count:** default-flag fold mirrors count::run all-four fallback ([6a76151](https://github.com/MikeGrier/tpu-rs/commit/6a76151777a059d0fdde7f5fc0ea54bf0829fe45))
* **count:** force stats=true and fix patterns schema description ([02b4bce](https://github.com/MikeGrier/tpu-rs/commit/02b4bceaadfb3d49f0cddb78ba01891c43ffa981))
* **extension:** collapse configuration array to single object so gettingStarted link renders ([5db2033](https://github.com/MikeGrier/tpu-rs/commit/5db2033e8b235b0f9fc16732b1b666e5c84ded29))
* fast-path FFFD scan; use Unicode escapes in test ([1052fb4](https://github.com/MikeGrier/tpu-rs/commit/1052fb43d6e7c00659dff93fb2dae7973db73871))
* tpu replace CLI supports --literal-replacement; clarify escaping note ([2342f71](https://github.com/MikeGrier/tpu-rs/commit/2342f71228764b74d1a0a554e766cbbfd91d4e23))
* **tpu-mcp:** fix label alignment and pattern metric isolation in call_count_file ([971db63](https://github.com/MikeGrier/tpu-rs/commit/971db6328e3e84dcce1859fb6a9c595a2d174209))
* **tpu-mcp:** guarantee NDJSON validity after diff output; clarify preview-mode trailers in docs ([9880a6a](https://github.com/MikeGrier/tpu-rs/commit/9880a6aaa8197287f0de86cda2e6fe89f9a87b88))
* **tpu-mcp:** make tpu_count_file emit x-tpu-mcp-result structured JSON ([9da8241](https://github.com/MikeGrier/tpu-rs/commit/9da824128d514324f5dcd57968edddfacb6b34d5))
* **tpu-mcp:** prevent standard-metric / pattern-label collisions in call_count_file ([140f7f9](https://github.com/MikeGrier/tpu-rs/commit/140f7f9840827f120a720ac1da494aa7fe53b148))
* **tpu-mcp:** return Ok(ToolResult) for unknown tools; update call() doc ([4c02b7b](https://github.com/MikeGrier/tpu-rs/commit/4c02b7b578c480cf2dff12cbc5fe8d1301fa79ce))
* **tpu-mcp:** skip blank line in dry_run with no diff; split read vs find in output docs ([6423125](https://github.com/MikeGrier/tpu-rs/commit/6423125a9b731fb3f41965b55949db3a0e230146))
* use .as_str() for context field in json! calls ([54acc1e](https://github.com/MikeGrier/tpu-rs/commit/54acc1e968820168dc71d632a933490d35ff4fcf))
* **worker:** remove redundant type cast on Ok(None) return ([9e211ce](https://github.com/MikeGrier/tpu-rs/commit/9e211ce275e3e40dc6e8b31806bd0791eed9fa91))

## [0.3.1](https://github.com/MikeGrier/tpu-rs/compare/v0.3.0...v0.3.1) (2026-06-12)


### Bug Fixes

* **extension:** split configuration into two blocks for deterministic ordering ([5c6f280](https://github.com/MikeGrier/tpu-rs/commit/5c6f280b6740dce876c69288b583859472c6ed1b))
* **extension:** surface Copilot setup chat in extension settings ([5e3307b](https://github.com/MikeGrier/tpu-rs/commit/5e3307b13b14a461bb20940ece9cd82025a7fc8f))

## [0.3.0](https://github.com/MikeGrier/tpu-rs/compare/v0.2.0...v0.3.0) (2026-06-12)


### Features

* **find:** add explicit glob parameter to filter directory walks ([bb16e6a](https://github.com/MikeGrier/tpu-rs/commit/bb16e6a322d4e8d764cd2f869521a21e5923316b))


### Bug Fixes

* **find:** name both CLI --glob and MCP glob: in directory-error hints ([5e3d595](https://github.com/MikeGrier/tpu-rs/commit/5e3d595b0e88bcca1d79a2c65785694cdf0268f7))

## [0.2.0](https://github.com/MikeGrier/tpu-rs/compare/v0.1.5...v0.2.0) (2026-06-08)


### Features

* force a feature version bump ([#30](https://github.com/MikeGrier/tpu-rs/issues/30)) ([9315ffd](https://github.com/MikeGrier/tpu-rs/commit/9315ffd20aad0ac0ed145d6e895ce07ee3ddffe5))

## [0.1.5](https://github.com/MikeGrier/tpu-rs/compare/v0.1.4...v0.1.5) (2026-05-14)


### Bug Fixes

* trigger release for copy/render/setup changes ([#25](https://github.com/MikeGrier/tpu-rs/issues/25)) ([f69ccb4](https://github.com/MikeGrier/tpu-rs/commit/f69ccb4aa28197f16de7bcc500d2b9a646b4bc4f))

## [0.1.4](https://github.com/MikeGrier/tpu-rs/compare/v0.1.3...v0.1.4) (2026-05-05)


### Bug Fixes

* add workspace-version action path filter; include root Cargo.toml in tpu change detection ([f5403cd](https://github.com/MikeGrier/tpu-rs/commit/f5403cd53b4d43fcc1e0f9712156a9a74f7cd456))
* add workspace-version action path filter; include root Cargo.toml in tpu change detection ([a690d75](https://github.com/MikeGrier/tpu-rs/commit/a690d7555c724fe35621f922585af5d0d879a4a1))
* address PR review feedback on release-please config and workflow messages ([8c03ae7](https://github.com/MikeGrier/tpu-rs/commit/8c03ae7453eac212f5e95be574fac5d1272ac852))
* address PR review feedback on release-please config and workflow messages ([93bf4ac](https://github.com/MikeGrier/tpu-rs/commit/93bf4ac066e5f045057b08e0fe09b988a13cbf1a))
* composite action for version, selective crate publish, update stale tag docs ([902b017](https://github.com/MikeGrier/tpu-rs/commit/902b0177165f4e0183b6c9a3d2e2e8ee12d77824))
* composite action for version, selective crate publish, update stale tag docs ([55113bb](https://github.com/MikeGrier/tpu-rs/commit/55113bbce9a05bf909d3ab4c0d5d6f4ff1e36f23))
* concurrency control, PAT token, and stale tag-format comments in workflows ([6c4b1b3](https://github.com/MikeGrier/tpu-rs/commit/6c4b1b37a8cd97ef00a492c6377742d662b09719))
* concurrency control, PAT token, and stale tag-format comments in workflows ([97c03c7](https://github.com/MikeGrier/tpu-rs/commit/97c03c7f5f1f59622e1ae47cb5f23e57c5e46105))
* correct retry_io doc comment and normalize find glob suggestion ([499daf4](https://github.com/MikeGrier/tpu-rs/commit/499daf4fe8c0d806b4ada57671b8bef9eff3dafa))
* create parent dirs before temp file in write; bump all versions … ([9791819](https://github.com/MikeGrier/tpu-rs/commit/97918193432fe48aec395dd2faa39af2cd6dec6d))
* create parent dirs before temp file in write; bump all versions to 0.1.3 ([2c15d98](https://github.com/MikeGrier/tpu-rs/commit/2c15d9809e49429839f86b34a44d7c80e09f6b42))
* doc precision, named error constants, expand_paths edge cases, tests ([d93ea91](https://github.com/MikeGrier/tpu-rs/commit/d93ea9177bd710f72eb71a2ee47351554969b133))
* fork-safe concurrency (PR number), tag-only version checks, anchored grep ([04e5e88](https://github.com/MikeGrier/tpu-rs/commit/04e5e88914e0161c24dec86224511a89fd1f801d))
* fork-safe concurrency (PR number), tag-only version checks, anchored grep ([4deff34](https://github.com/MikeGrier/tpu-rs/commit/4deff344601a31cd15cc665453bb3244c94b5f33))
* grep || true, targeted Cargo.toml dep check, fork-safe concurrency groups ([5ccb17b](https://github.com/MikeGrier/tpu-rs/commit/5ccb17b01a61d50beec3cacb2e4fd48ee14c20ae))
* grep || true, targeted Cargo.toml dep check, fork-safe concurrency groups ([fbf46b6](https://github.com/MikeGrier/tpu-rs/commit/fbf46b6141c4e28b5580f1dd7e95545ceec58916))
* migration tag fallback, dynamic workspace-key detection, package.json check ([c0b3f7e](https://github.com/MikeGrier/tpu-rs/commit/c0b3f7ee6e970386c37dec3b7469da5e7ef6053a))
* migration tag fallback, dynamic workspace-key detection, package.json check ([d211aba](https://github.com/MikeGrier/tpu-rs/commit/d211aba266361e7ef57bd4f218dc974b5b1077f9))
* POSIX awk, selective-publish version filter, pkg.json check, CI concurrency ([12be8ab](https://github.com/MikeGrier/tpu-rs/commit/12be8abe485001e821b00a7a2d8ea7a3ff11d5c0))
* POSIX awk, selective-publish version filter, pkg.json check, CI concurrency ([2cc5eac](https://github.com/MikeGrier/tpu-rs/commit/2cc5eac81bb42c73819d285aa21a805c143fe56a))
* retry tmp.persist(file) with retry_io in all write paths ([7600e53](https://github.com/MikeGrier/tpu-rs/commit/7600e536786bac9da8944f78e7823b56e4863fee))
* retry transient AV/Defender I/O errors; clear find directory error ([c9f2483](https://github.com/MikeGrier/tpu-rs/commit/c9f248322e053e29c4342844c653e57e5758111b))
* retry transient AV/Defender I/O errors; clear find directory error ([ceebb5c](https://github.com/MikeGrier/tpu-rs/commit/ceebb5ccfbf99fe84703721986c74d04f68ad156))
* use try_exists() instead of exists() in write; add nested-path regression test ([94aab1e](https://github.com/MikeGrier/tpu-rs/commit/94aab1e55febac12f512ab98ef22a7f06dcab73c))
