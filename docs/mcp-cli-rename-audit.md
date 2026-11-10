# Project rename audit

This is the historical pre-rename inventory at `46bc986`. The current CLI is
`mcp-cli`; see the [upgrade guide](installation.md#upgrade-from-mcp-pool) for
current names, retained compatibility contracts, and daemon cutover instructions.
The baseline counts and source-line appendix below intentionally remain unchanged.

## Rename implementation verification

The package, executable, CLI/protocol display names, installers, current docs,
automation, sockets/pipes, and diagnostic log now use `mcp-cli`.
`src/environment.rs` owns old-name runtime aliases and restrictive read-only policy.
`tests/rename_compatibility.rs` covers seven public compatibility scenarios.
Remaining old names identify compatibility contracts, pool domain terms, or history.

Verified against source fingerprint `73bbfd409fc28612`:

- `cargo build --release --locked` produced `target/release/mcp-cli`.
- `cargo test --locked -- --test-threads=1` passed 616 tests on macOS and Boat Linux.
- `cargo clippy --all-targets --locked -- -D warnings` passed on both hosts.
- `cargo fmt --all -- --check` and 31 Python tests passed on both hosts.
- The release CLI passed all 126 local UX scenarios. The separate mcporter reference
  retained four reported differences; none failed the product gate.
- Benchmark self-test passed stdio/HTTP and legacy/modern protocol cases.
  All 11 owned benchmark namespaces were removed without cleanup errors.
- Boat's copied source had the same source fingerprint. Its missing Git metadata
  correctly selected the supported `nogit` build-identity fallback.

Only task-owned fixtures were exercised. No installed user executable, credentials,
live user daemon, GitHub repository name, origin URL, or checkout folder was changed.
Native Windows execution remains unverified. The existing Windows installer and
named-pipe tests are updated but require a Windows host.

## Scope and result

Users would invoke `mcp-cli` instead of `mcp-pool`. Existing agent configurations,
shell scripts, installed binaries, and daemon state need a coordinated cutover.
Maintainers inherit both a branding change and compatibility decisions, not just
a Cargo package rename.

The original audit changed no application names, runtime state, credentials,
repository settings, or existing source files. Subsequent rename implementation
is separate from this baseline report.

`git pull --ff-only origin main` reported already up to date. Local `main` and
`origin/main` both resolved to `46bc9869547094da43d60b82e68c63380a45be35`.
All line numbers below refer to that commit.

Measured inventory across 250 tracked paths, including hidden configuration:

- 112 text files contain the old name stem.
- 533 matching lines contain 570 occurrences.
- Spellings comprise 360 `mcp-pool`, 162 `MCP_POOL`, 36 `mcp_pool`, and 12 `MCP pool` occurrences.
- 35 distinct `MCP_POOL_*` identifiers include runtime settings, build metadata, and test fixtures.
- One tracked filename contains the old brand, `plans/001-mcp-pool-reliability-parity.md`.

These are search results, not 570 mandatory replacements. Legacy formats,
persisted metadata, and domain language need different treatment.

## Reproduce the complete inventory

The read-only scanner includes exact paths, line numbers, source text, spelling
counts, filename matches, and environment-variable locations. It uses Git, not a
stale code-index cache. It requires Python and Git, with no extra packages.

```sh
python3 scripts/project_rename_audit.py --ref 46bc9869547094da43d60b82e68c63380a45be35
python3 scripts/project_rename_audit.py --format lines
python3 scripts/project_rename_audit.py --format json
python3 scripts/project_rename_audit.py --worktree --format lines
python3 -m unittest discover -s scripts -p 'test_project_rename_audit.py' -v
```

Default scans committed `HEAD`. `--worktree` scans tracked working-tree contents,
not untracked files. The audit report and scanner deliberately contain old names;
exclude or classify those references when judging completion after committing them.
The appendix can be regenerated with `--format markdown` at the audited revision.

The six scanner tests passed locally. They cover hidden files, spelling variants,
counts, filename-only findings, binary exclusion, snapshot/worktree separation,
untracked exclusion, invalid revisions, and JSON/Markdown output.
An independent broader Git expression found the same 533 matching lines.
`cargo metadata --offline --locked --no-deps` confirmed package/target structure.
No Rust behavior changed, so the Rust suite and Boat CI were not run for this audit.

## 1. Cargo package, executable, and build identity

| Owner | Required review/update |
| --- | --- |
| `Cargo.toml:2` | Change package name to `mcp-cli`. Review description if product positioning also changes. |
| `Cargo.lock:785` | Update the root package entry through Cargo without upgrading unrelated dependencies. |
| `build.rs:38,45,78,85` | Update build-warning labels and decide whether build-time `MCP_POOL_BUILD_VERSION` becomes `MCP_CLI_BUILD_VERSION`. |
| `src/cli.rs:22,23` | Change Clap command name and match the build-version variable emitted by `build.rs`. |
| `tests/build_identity.rs:6,15,21,23,26,28` | Update executable lookup, expected version prefix, and build-version variable together. |
| Integration-test executable lookups | Replace every `env!("CARGO_BIN_EXE_mcp-pool")` with `env!("CARGO_BIN_EXE_mcp-cli")`. Appendix identifies each file. |

Cargo metadata confirms one package and one automatically inferred application
binary. There is no explicit `[[bin]]`, workspace package split, or library target.
The package rename therefore changes the inferred binary to `mcp-cli`, build
outputs to `target/{debug,release}/mcp-cli`, and Cargo's integration-test executable
variable. Windows outputs gain the corresponding `.exe` name.

No old-name crate imports or `mcp_pool` Rust module declarations were found.
`src/mcp_cli.rs`, `tests/mcp_cli.rs`, and `tests/mcp_cli_fixtures/` already describe
MCP CLI behavior. They do not need filename changes for the new brand.

The package has `publish = false`. Cargo metadata has no repository, homepage,
or documentation URL. Registry publishing is not currently configured; do not
turn it on as an accidental side effect of renaming.

## 2. Installers, CI, hooks, audit runner, and benchmarks

| Owner | Required review/update |
| --- | --- |
| `install.sh:15,49,50,81,137,139` | Installer env override, built/destination filenames, staging prefix, and PATH lookup/warnings. |
| `install.ps1:34,40,48,87,107,120,122,127` | Source/destination `.exe`, staging name, errors, and `Get-Command` lookup/warnings. |
| `tests/installer.rs` and `tests/installer_unix.rs` | Staged and destination filenames, Cargo executable lookup, and version verification. |
| `.github/workflows/unix-ipc.yml:29-33` | Isolated-home/read-only env names and `cargo test --bin mcp-pool`. Preserve Boat runner and cleanup. |
| `scripts/pre_push.py:80,88-90` | Both Cargo `--bin` arguments and isolated-home/read-only env names. |
| `scripts/test_pre_push.py` | Stub command expectations and shell/Python fixture env accesses. |
| `scripts/cli_ux_audit.py` | CLI `--mcp-pool` option, `arguments.mcp_pool`, executable default, product keys, branches, expected help, env settings, and result selection. |
| `scripts/test_cli_ux_audit.py` | Product labels and transcript filenames, not just command text. |
| `benchmarks/run.py` | `--mcp-pool`, `--products mcp-pool`, defaults, help, product labels, and result metadata. |
| `benchmarks/runtime.py` | Product-dependent daemon launch/status logic and isolated-home/read-only settings. |
| `benchmarks/bridge.py:41` | Product-dependent concurrency flag handling. |

Python product names are control flow, not cosmetic labels. Renaming only an
argument or output heading would leave branches selecting mcporter behavior.
In the benchmark runner, product labels also derive attribute names through
`product.replace("-", "_")`; option destinations and lookups must agree.
Decide whether old runner flags remain aliases and whether historical result
consumers accept new product keys. Do not rewrite past measurement artifacts.

`dev.sh`, `dev.ps1`, `bacon.toml`, `.githooks/pre-push`, and
`scripts/install-hooks.sh` have no literal old-brand dependency. Their generic
Cargo commands or relative script paths already follow the checkout/package.
No tracked release-upload workflow, npm package, Docker image definition,
Homebrew formula, Scoop manifest, or winget manifest was found.

## 3. CLI help, errors, generated commands, and protocol-visible names

| Surface | Owners |
| --- | --- |
| Root and native namespace usage | `src/cli.rs:22,182,430`, `src/cli_routing.rs:17`, `src/cli_help.rs` throughout. |
| Configuration/auth remediation text | `src/config_output.rs:132,135`, `src/tool_discovery_error.rs:16`, `src/oauth.rs:339,349`, `src/upstream_http_options.rs:80,82`. |
| Generated executable tool examples | `src/tool_documentation.rs:263`. |
| Import warnings and server-config guidance | `src/config_import.rs:17`, `src/config_import_convert.rs:242`, `src/server_config.rs:286`. |
| Native daemon status/launch guidance | `src/daemon_client.rs:195`, `src/daemon_commands.rs:265`, `src/daemon_launch.rs:16,64`. |
| stderr branding and artifact notices | `src/adhoc_persistence.rs`, `src/cli_context.rs`, `src/cli_output.rs`, `src/config_permissions.rs`, `src/config_writer.rs`, `src/diagnostics.rs`, `src/output_artifacts.rs`, `src/terminal_color.rs`, `src/tool_call.rs`. |
| HTTP bridge startup and SSE/stdio errors | `src/mcp_bridge_http.rs`, `src/mcp_bridge_http_sse.rs`, `src/mcp_bridge_stdio.rs`. |
| Upstream legacy initialization `clientInfo.name` | `src/mcp_client.rs:129`. |
| Downstream legacy and modern `serverInfo.name`, plus bridge instructions | `src/mcp_bridge_rpc.rs:173,272-308`. |
| Default OAuth registration `client_name` | `src/oauth.rs:292`. |

Update exact expectations with their owners. Particularly important are
`src/mcp_bridge_test_fixture.rs:290`, bridge modern/legacy response tests,
`src/tool_documentation_tests.rs`, `src/config_output_tests.rs`,
`tests/cli_presentation.rs`, `tests/cli_routing.rs`, and
`tests/mcp_cli_fixtures/{admin,bridge_notifications,discovery_report,presentation}.rs`.
The bridge-notification fixture parses a branded stderr prefix to discover its URL.
Missing that update can break fixture readiness, not just string assertions.

Daemon spawning uses `std::env::current_exe()` on Unix and Windows in
`src/daemon_launch.rs`. No hardcoded executable lookup needs replacing there;
only its diagnostic strings are branded.

Changing default OAuth display name should not discard existing registered
client IDs or force consent. `src/oauth.rs:251-273` derives credential keys from
server name and URL or command/args, not the product display name. However,
`HttpAuth.client_name` is serialized inside the resolved server definition, which
`src/pool_identity.rs:9-25` hashes. Inferred impact is a different pool identity
for OAuth-enabled definitions when the default display name changes. Include
that in the daemon cutover even if socket prefixes are retained.

## 4. Environment-variable contract

| Existing identifier | Proposed spelling if renamed | Owners |
| --- | --- | --- |
| `MCP_POOL_HOME` | `MCP_CLI_HOME` | `src/config.rs:7`, `src/config_paths.rs:6`, Windows pipe scoping, tests, CI, hooks, benchmarks, docs. |
| `MCP_POOL_CONFIG` | `MCP_CLI_CONFIG` | `src/config_paths.rs:45`, test env cleanup, docs. |
| `MCP_POOL_DEBUG` | `MCP_CLI_DEBUG` | `src/diagnostics.rs:30`, CLI help, tests, docs. |
| `MCP_POOL_NO_PROGRESS` | `MCP_CLI_NO_PROGRESS` | `src/cli_progress.rs:31`, presentation fixture, docs. |
| `MCP_POOL_RAW_UPSTREAM_STDERR` | `MCP_CLI_RAW_UPSTREAM_STDERR` | `src/upstream_stdio.rs:196`. This runtime setting is not currently documented in README. |
| `MCP_POOL_CREDENTIALS_READ_ONLY` | `MCP_CLI_CREDENTIALS_READ_ONLY` | `src/oauth.rs:305,307,346,349`, `src/server_config_auth.rs:45,48`, HTTP auth errors, isolated test/runner settings, docs. |
| `MCP_POOL_BIN_DIR` | `MCP_CLI_BIN_DIR` | `install.sh:15`. PowerShell uses its `-BinDir` parameter instead. |
| `MCP_POOL_BUILD_VERSION` | `MCP_CLI_BUILD_VERSION` | Build-time producer/consumers, not runtime user configuration. |
| `MCP_POOL_MIGRATION_DIRECTORY` | `MCP_CLI_MIGRATION_DIRECTORY` | `src/legacy_directories.rs:105,123`. Internal Rust-to-PowerShell handoff; update both ends. |

The remaining 26 distinct identifiers are fixture/synthetic variables. Update
writers and readers together, including embedded Python, shell, and PowerShell
strings. Full variable locations are available in scanner JSON.

Compatibility decision required before implementation. Either preserve existing
runtime env names, or introduce new names with explicitly documented old-name
aliases and precedence. Dropping old names silently can select the wrong config
or lose fixture isolation. Treat old read-only restrictions as active during
transition; an alias must not silently enable credential refresh or writes.

Do not rename external-tool contracts such as `MCPORTER_CONFIG`,
`MCPORTER_DAEMON_DIR`, `MCPORTER_NO_SPINNER`, or provider/editor env settings.
Those identify interoperability, not this project's brand.

## 5. Runtime endpoints, persisted formats, and legacy compatibility

These are the highest-risk replacements. They must not follow a global replace.

| Existing value | Owner | Recommendation |
| --- | --- | --- |
| `mcp-pool-control.sock` | `src/config.rs:48` | Choose whether rename includes control endpoint. If yes, stop old owner before new launch and update direct-socket tests. |
| `mcp-pool-{safe}.sock` | `src/config.rs:61` | Update together with the Unix discovery parser in `src/pool.rs:287`. |
| `\\.\pipe\mcp-pool-{scope}-control` and server variant | `src/config.rs:53,66` | Update both named-pipe constructors and Windows fixture lookups. Retain home scoping. |
| `mcp-pool.log` | `src/diagnostics.rs:79` | Rename diagnostic logfile only with docs, readers, and tests. Existing `daemon.log` is separate and unbranded. |
| `.mcp-pool-...pending` | `src/oauth_files.rs:101` | Temporary-write prefix can change without rewriting stored credentials. |
| `.mcp-pool.install...` | Both installers | Temporary installer prefix can change with staged executable names. |
| `mcp-pool` directory recognition | `src/local_security_unix.rs:299,305` | This selects private-directory verification/repair policy. Retain legacy recognition; add new recognition only if new runtime directories need it. |
| `__mcp_pool_commit` | `src/oauth_reconcile.rs:7`, `src/oauth_import.rs:183` | Keep persisted credential generation key. Renaming would need explicit migration across reconciliation and import comparison. |
| `_mcp_pool_timeout_ms` | `src/request_deadline.rs:6` and timeout/encoding fixtures | Keep existing private wire extension unless deliberately versioning/migrating the local protocol. |
| `mcp-pool-legacy-v1` | `src/daemon_migration.rs:73,224` | Keep existing retirement-journal format tag. Old incomplete journals must remain readable. |
| `mcp-pool-migration-...` | `src/daemon_migration.rs:289` | Correlation-ID prefix can be rebranded; not equivalent to changing journal format. |
| `import mcp-pool` | `src/config_import_kind.rs:27,68` | Keep source selector identifying former native TOML. New invocation can correctly be `mcp-cli import mcp-pool`. |
| Platform config directory `mcp-pool/config.toml` | `src/config_import_sources.rs:146` | Keep historical source lookup. Renaming it would prevent finding old installations. |

Default live data already uses `~/.mcp/` and project `.mcp/`, not an application
folder named `mcp-pool`. Preserve `config.json`, `credentials.json`, `run/`,
`logs/`, explicit credential scopes, and `tokenCacheDir`. A new brand does not
require moving user data. Override layouts remain `HOME_OVERRIDE/config/` and
`HOME_OVERRIDE/state/` regardless of env-var spelling.

Inferred failure if socket prefixes change without draining old processes:
new clients cannot see the old control endpoint and can launch a second daemon,
which can spawn duplicate upstreams or race on shared credentials. Even with
unchanged prefixes, automatic reattachment could leave a renamed CLI using old
daemon code. Prefer an explicit one-time stop/start cutover for either policy.
`daemon migrate` inventories mcporter legacy directories in
`src/legacy_directories.rs:5-28`; it is not an existing old-brand daemon migration.

`Pool`, `pool.rs`, `pool_identity.rs`, `PoolStatusResponse`, `socket_proxy*`,
the `pool` subcommand, `pool_*` log events, and the internal `mcp-` identity
prefix remain valid domain terms. Do not rename them to `cli` merely because
the project name changed. Likewise, descriptions such as “MCP pool request
failed” are not necessarily product-brand strings.

## 6. Documentation and filename policy

- `README.md` owns title, install/build paths, hook commands, examples, env
  contracts, bridge/daemon usage, agent configuration, and performance-runner flags.
- `AGENTS.md` owns project identity, executable examples, env and socket contracts,
  and smoke commands. Update alongside implementation so future agents use new CLI.
- `CLAUDE.md` only points to `AGENTS.md`; no literal rename is needed.
- `docs/cli-ux-audit.md` owns reproduction commands, product labels, and runner flags.
- `CHANGELOG.md` has five matching lines, all currently under `Unreleased`.
  Review branding during release preparation, per project policy. This audit does
  not edit the changelog or create a release entry.
- `plans/README.md` and `plans/001-mcp-pool-reliability-parity.md` document completed
  work, old commands, captured logs, and historical platform paths. Preserve factual
  historical references or label them historical instead of rewriting evidence.
  Renaming the plan filename is optional; if chosen, update incoming references.

Only the plan filename contains the old brand among tracked paths. Existing Rust
source filenames do not require mass renaming. Audit prose and intentional
compatibility strings should remain explicit exceptions to any future name gate.

## 7. Outside tracked source

Observed local origin is `https://github.com/BumpyClock/mcp-pool.git`.
No matching application repository URL is stored in tracked source.

Separate operator work, not performed or verified by this audit:

- Rename GitHub repository to `BumpyClock/mcp-cli` if repository rename is intended.
  Review description, topics, website, branch protection, integrations, badges,
  external links, release names/assets, and package-distribution references.
- Update clone remotes, local folder name, IDE recent projects, and external agent
  workspace configuration. The current checkout directory is still `mcp-pool`.
- Review Boat's external repository/actor configuration. The tracked workflow uses
  `runs-on: boat` but contains no repository-specific runner name to replace.
- Install new executable on each host. Decide whether an old-name compatibility
  launcher is needed. Updating scripts alone does not remove installed old binaries.
- Update agent MCP `command` entries, shell aliases/functions, PATH-selected copies,
  scheduled jobs, service definitions, and downstream automation using old executable
  names or env variables. External config sources are intentionally not auto-edited.
- Check destination repository ownership and package-name availability before any
  publication. This scan establishes neither availability nor trademark clearance.

Ignored local artifacts include `target`, `.test-artifacts/`, benchmark results,
Python caches, and Ruff cache. `target` is a symlink to an external build cache on
this machine. Root listing also showed three old-name lifecycle sockets, outside
the tracked inventory. Do not follow the build-cache link, rename live sockets,
or rewrite historical artifacts for branding. No cleanup or process stop was done.

## 8. Recommended implementation sequence and acceptance

1. Decide compatibility scope. Recommend changing package, executable, current
   docs/help/log branding, and new MCP/OAuth display names; preserve `.mcp` data
   paths and historical formats. Decide env aliases and runtime endpoint policy.
2. Rename package/build identity, installers, Cargo target references, test executable
   lookups, and Python product/option branches together. Update lockfile root entry
   without unrelated dependency upgrades. Check with locked Cargo commands afterward.
3. Update user-facing strings, generated auth/tool commands, protocol identities,
   associated assertions, documentation, and chosen env/endpoint changes.
4. Perform approved cutover separately. Drain clients and stop old daemons through
   old executable with each original home override before launching renamed runtime.
   Verify retirement; do not kill arbitrary matching process names.
5. Verify in isolated homes with synthetic credentials. Test new `--version`, root
   and nested help, installer staging/PATH behavior, both proxy forms, daemon
   start/status/stop/restart, bridge stdio/HTTP legacy/modern identities, discovery,
   generated commands, timeouts, and env precedence/read-only preservation.
6. Retain legacy TOML import, old credential generation metadata, and journal
   fixtures. Confirm two concurrent proxies still share one upstream. Qualify
   Unix security/socket behavior on Boat and Windows named-pipe/process behavior
   natively. Run `cargo fmt --all -- --check`, `cargo build --locked`,
   `cargo test --locked`, `cargo clippy --all-targets --locked`, Python hook/audit
   tests, and isolated audit/benchmark self-tests using renamed runner arguments.
7. Rerun name inventory. Review each remaining old-brand reference as historical,
   compatibility, or audit evidence. Do not require zero raw matches.

## Appendix. Complete tracked-content inventory

Occurrences count the name stem, including inside env names and metadata keys.
Line numbers are from the audited commit, not this new report. Full source text
for every line is available through scanner `--format lines` or `--format json`.

| File | Occurrences | Matching line numbers |
| --- | ---: | --- |
| `.github/workflows/unix-ipc.yml` | 5 | 29, 30, 31, 32, 33 |
| `AGENTS.md` | 14 | 1, 4, 11, 16, 191, 204, 206, 207, 208, 209, 210, 212, 221, 238 |
| `CHANGELOG.md` | 5 | 35, 48, 69, 76, 129 |
| `Cargo.lock` | 1 | 785 |
| `Cargo.toml` | 1 | 2 |
| `README.md` | 93 | 1, 12, 18, 37, 70, 82, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 121, 154, 164, 183, 184, 185, 186, 187, 188, 189, 190, 191, 201, 202, 203, 204, 205, 247, 352, 366, 367, 368, 369, 370, 371, 372, 373, 374, 375, 376, 425, 430, 432, 434, 435, 436, 437, 440, 451, 452, 453, 454, 455, 461, 467, 468, 469, 470, 481, 525, 526, 527, 528, 562, 563, 564, 646, 647, 688, 691, 694, 695, 696, 697, 698, 699, 702, 705, 709, 712 |
| `benchmarks/bridge.py` | 1 | 41 |
| `benchmarks/run.py` | 7 | 30, 32, 33, 41, 195, 209 |
| `benchmarks/runtime.py` | 4 | 100, 101, 163, 174 |
| `build.rs` | 4 | 38, 45, 78, 85 |
| `docs/cli-ux-audit.md` | 7 | 5, 13, 20, 26, 76 |
| `install.ps1` | 8 | 34, 40, 48, 87, 107, 120, 122, 127 |
| `install.sh` | 6 | 15, 49, 50, 81, 137, 139 |
| `plans/001-mcp-pool-reliability-parity.md` | 22 | 1, 36, 38, 90, 126, 127, 145, 447, 452, 457, 472, 479, 480, 502, 522 |
| `plans/README.md` | 2 | 11, 22 |
| `scripts/cli_ux_audit.py` | 16 | 2, 71, 185, 283, 284, 318, 341, 349, 375, 387, 393, 407, 411 |
| `scripts/pre_push.py` | 4 | 80, 88, 89, 90 |
| `scripts/test_cli_ux_audit.py` | 9 | 28, 35, 58, 83, 89, 126, 135, 142, 156 |
| `scripts/test_pre_push.py` | 6 | 15, 16, 40, 60, 62, 234 |
| `src/adhoc_persistence.rs` | 1 | 48 |
| `src/cli.rs` | 10 | 22, 23, 25, 35, 82, 92, 182, 414, 415, 430 |
| `src/cli_context.rs` | 1 | 154 |
| `src/cli_help.rs` | 38 | 8, 11, 14, 17, 20, 23, 26, 29, 32, 35, 39, 41, 47, 50, 53, 56, 59, 62, 65, 68, 71, 74, 77, 86, 100 |
| `src/cli_output.rs` | 2 | 76, 82 |
| `src/cli_progress.rs` | 1 | 31 |
| `src/cli_routing.rs` | 2 | 17, 18 |
| `src/config.rs` | 9 | 7, 48, 53, 61, 66, 195, 208, 218, 222 |
| `src/config_import.rs` | 1 | 17 |
| `src/config_import_convert.rs` | 1 | 242 |
| `src/config_import_kind.rs` | 3 | 27, 33, 68 |
| `src/config_import_sources.rs` | 1 | 146 |
| `src/config_output.rs` | 2 | 132, 135 |
| `src/config_output_tests.rs` | 4 | 34, 38, 61, 167 |
| `src/config_paths.rs` | 2 | 6, 45 |
| `src/config_permissions.rs` | 1 | 119 |
| `src/config_writer.rs` | 1 | 45 |
| `src/configuration_reader_tests.rs` | 3 | 253, 259, 450 |
| `src/daemon_client.rs` | 1 | 195 |
| `src/daemon_commands.rs` | 1 | 265 |
| `src/daemon_launch.rs` | 2 | 16, 64 |
| `src/daemon_migration.rs` | 3 | 73, 224, 289 |
| `src/diagnostics.rs` | 5 | 30, 65, 73, 79, 82 |
| `src/legacy_directories.rs` | 2 | 105, 123 |
| `src/local_security_unix.rs` | 2 | 299, 305 |
| `src/local_security_unix_tests.rs` | 4 | 167, 180, 236, 302 |
| `src/local_security_windows_tests.rs` | 4 | 12, 140, 141, 142 |
| `src/mcp_bridge.rs` | 1 | 61 |
| `src/mcp_bridge_http.rs` | 2 | 71, 83 |
| `src/mcp_bridge_http_modern_tests.rs` | 3 | 87, 89, 316 |
| `src/mcp_bridge_http_sse.rs` | 3 | 166, 206, 213 |
| `src/mcp_bridge_rpc.rs` | 8 | 173, 182, 272, 274, 281, 291, 293, 308 |
| `src/mcp_bridge_stdio.rs` | 1 | 83 |
| `src/mcp_bridge_test_fixture.rs` | 1 | 290 |
| `src/mcp_bridge_tests.rs` | 2 | 84, 85 |
| `src/mcp_client.rs` | 2 | 69, 129 |
| `src/oauth.rs` | 6 | 292, 305, 307, 339, 346, 349 |
| `src/oauth_authorization_tests.rs` | 1 | 185 |
| `src/oauth_files.rs` | 1 | 101 |
| `src/oauth_import.rs` | 1 | 183 |
| `src/oauth_reconcile.rs` | 1 | 7 |
| `src/oauth_store_tests.rs` | 3 | 258, 278, 303 |
| `src/oauth_tests.rs` | 2 | 53, 74 |
| `src/output_artifacts.rs` | 4 | 22, 76, 97, 110 |
| `src/pool.rs` | 7 | 287, 300, 301, 302, 303, 304, 306 |
| `src/pool_identity.rs` | 2 | 63 |
| `src/proxy_retry_tests.rs` | 3 | 13, 20, 52 |
| `src/request_deadline.rs` | 1 | 6 |
| `src/server_config.rs` | 1 | 286 |
| `src/server_config_auth.rs` | 2 | 45, 48 |
| `src/socket_proxy_deadline_tests.rs` | 5 | 39, 73, 125, 188, 225 |
| `src/socket_proxy_encoding_tests.rs` | 21 | 14, 21, 31, 37, 83, 109, 113, 117, 121, 125, 143, 164, 185, 190, 196, 235, 249, 284, 296, 301 |
| `src/socket_proxy_lifecycle_tests.rs` | 2 | 16, 19 |
| `src/socket_proxy_shared_deadline_tests.rs` | 2 | 346, 354 |
| `src/terminal_color.rs` | 1 | 9 |
| `src/terminal_color_tests.rs` | 1 | 21 |
| `src/tool_call.rs` | 1 | 107 |
| `src/tool_discovery_error.rs` | 1 | 16 |
| `src/tool_documentation.rs` | 1 | 263 |
| `src/tool_documentation_tests.rs` | 4 | 190, 246, 286, 309 |
| `src/upstream_http_auth_tests.rs` | 4 | 293, 295, 340 |
| `src/upstream_http_deadline_tests.rs` | 4 | 11, 17, 184, 335 |
| `src/upstream_http_options.rs` | 3 | 80, 82 |
| `src/upstream_http_ownership_tests.rs` | 1 | 55 |
| `src/upstream_http_tests_retirement.rs` | 1 | 161 |
| `src/upstream_process_tests.rs` | 3 | 10, 365, 384 |
| `src/upstream_stdio.rs` | 7 | 196, 317, 326, 331, 374, 386, 393 |
| `src/upstream_stdio_windows_tests.rs` | 8 | 65, 119, 120, 140, 141, 159, 187, 202 |
| `tests/build_identity.rs` | 7 | 6, 15, 21, 23, 26, 28 |
| `tests/cli_lifecycle.rs` | 11 | 14, 25, 30, 31, 55, 66, 78, 79, 97, 98, 356 |
| `tests/cli_presentation.rs` | 5 | 32, 46, 220, 223, 285 |
| `tests/cli_routing.rs` | 5 | 25, 37, 167, 170, 289 |
| `tests/config_import.rs` | 3 | 73, 236, 278 |
| `tests/config_import_auth.rs` | 4 | 49, 93, 173, 282 |
| `tests/config_import_copy_from.rs` | 5 | 165, 166, 184, 190, 194 |
| `tests/config_import_fixtures/support.rs` | 5 | 8, 43, 48, 49, 104 |
| `tests/config_import_mcporter.rs` | 1 | 144 |
| `tests/default_paths.rs` | 7 | 29, 35, 36, 37, 87, 91, 102 |
| `tests/installer.rs` | 4 | 39, 40, 53, 101 |
| `tests/installer_unix.rs` | 4 | 38, 39, 52, 119 |
| `tests/mcp_cli.rs` | 2 | 68, 456 |
| `tests/mcp_cli_fixtures/admin.rs` | 2 | 266, 330 |
| `tests/mcp_cli_fixtures/bridge_notifications.rs` | 1 | 52 |
| `tests/mcp_cli_fixtures/discovery_report.rs` | 2 | 190, 313 |
| `tests/mcp_cli_fixtures/identity.rs` | 1 | 16 |
| `tests/mcp_cli_fixtures/ownership.rs` | 4 | 33, 76, 114, 194 |
| `tests/mcp_cli_fixtures/presentation.rs` | 4 | 35, 74, 84, 140 |
| `tests/mcp_cli_fixtures/pseudo_discovery.rs` | 1 | 15 |
| `tests/mcp_cli_fixtures/stdio.rs` | 9 | 8, 9, 10, 11, 48, 91, 122, 128, 143 |
| `tests/mcp_cli_fixtures/support.rs` | 10 | 10, 41, 81, 93, 175, 253, 254, 273, 274, 292 |
| `tests/protocol_negotiation.rs` | 3 | 18, 19, 20 |
| `tests/stdio_diagnostics.rs` | 12 | 11, 30, 33, 34, 48, 49, 62, 73, 85, 86, 126, 127 |
| `tests/tool_resolution.rs` | 8 | 68, 70, 74, 106, 389, 409, 475, 479 |
