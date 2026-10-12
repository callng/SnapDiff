# AGENTS.md

## Purpose and scope

This document guides AI coding agents working anywhere in the SnapDiff repository. Read it before changing code. More specific instructions in a nested `AGENTS.md`, if introduced later, apply within that directory. Explicit instructions from the current user take precedence over repository guidance.

SnapDiff is a Windows-first, local desktop directory-state comparison tool built with Rust and Tauri 2. It records directory structure, file metadata, and MD5 hashes, then compares manually recorded states. It is a general-purpose directory tool; games and Mod management are examples, not restrictions on its UI or behavior.

The frontend currently uses static HTML, CSS, and plain JavaScript. There is no React, Vue, Svelte, TypeScript, bundler, frontend compilation step, or production HTTP server. Do not introduce one solely for a small feature.

Keep this file in English. The existing product UI and user-facing errors are primarily Simplified Chinese; preserve that language and terminology unless the user requests localization. Documentation language is not a reason to translate the application.

## Product boundaries

Preserve these boundaries unless the user explicitly changes the product scope:

- Store metadata and hashes only. Reading file contents to calculate a hash is necessary, but contents must never be persisted as snapshots or copied into a backup store.
- Do not implement file restoration, backup, synchronization, filesystem watchers, scheduled scanning, or automatic rescanning.
- Do not add accounts, cloud services, telemetry, remote APIs, or runtime network features.
- Do not modify files in a scanned directory. Deleting a directory record or history node deletes application records only.
- Keep scanning and comparison in Rust. The frontend displays summaries and bounded result pages.
- Use MD5 as the required change fingerprint. Do not market it as cryptographic integrity, tamper detection, or proof of authenticity.
- Keep user operations explicit. Opening the app, a directory workspace, a history dialog, or the creation dialog must not start a scan.

Development dependency downloads and localhost preview tools are separate from production behavior.

## Working in the repository

1. Read `README.md`, inspect the relevant modules, and check `git status --short` before editing. Existing staged, unstaged, and untracked work may belong to the user or earlier tasks.
2. Treat source code, manifests, and tests as the current implementation. Validation documents describe specific past runs and may reflect older architecture.
3. Make focused edits. Do not reset, clean, stage, commit, remove IDE files, or reformat unrelated files as a side effect of a task.
4. Continue authorized implementation and routine verification without repeatedly asking for permission. Ask when a required product decision cannot be inferred, or a consequential action is outside the user's request.
5. Do not alter the user's installed application or real snapshot database to test a change. Use temporary directories and, for native UI verification, an independent Tauri application identifier.
6. Update the relevant documentation when behavior, schemas, commands, or maintenance assumptions change. Keep this guide accurate rather than appending obsolete rules indefinitely.

The usual host shell is PowerShell. Quote paths containing spaces, use literal paths for filesystem operations, and verify the absolute target before recursive removal. Do not hard-code a developer's username, home directory, game path, or machine-specific dependency cache into source or documentation examples.

## Repository map

| Path | Responsibility |
| --- | --- |
| `Cargo.toml` | Rust workspace; includes `src-tauri` |
| `Cargo.lock` | Workspace dependency lockfile |
| `package.json`, `package-lock.json` | Tauri CLI and frontend test dependencies |
| `frontend/index.html` | Static views, dialogs, startup markup, displayed version |
| `frontend/styles.css` | Dark desktop styling, responsive layout, loading animation |
| `frontend/core.js` | `window.SD`, shared state, invoke wrapper, escaping, dialogs, notifications, clipboard, default-menu suppression |
| `frontend/app.js` | Initialization, directory navigation, scan lifecycle, settings, import/export, native drop handling |
| `frontend/history.js` | History selection, activity grid, remarks, batch actions |
| `frontend/results.js` | Difference queries, pages, filters, details, tree navigation, reports |
| `src-tauri/src/main.rs` | Desktop executable entry point and Windows GUI subsystem |
| `src-tauri/src/lib.rs` | Plugins, startup wiring, backend state, command registration |
| `src-tauri/src/startup_window.rs` | Gate joining page readiness and restored window geometry |
| `src-tauri/src/models.rs` | Public models, summaries, entry/status types, public JSON schema version |
| `src-tauri/src/scanner.rs` | Recursive enumeration, strict streaming MD5, progress, cancellation, file-stability checks |
| `src-tauri/src/ignore.rs` | Ordered ignore/focus rule matching and effective scan scope |
| `src-tauri/src/storage.rs` | Persistent SQLite store, directory identity, transactions, retention, migration, import/export |
| `src-tauri/src/entry_codec.rs` | Internal SQLite entry encoding and database schema migration |
| `src-tauri/src/sql_diff.rs` | Production comparison, disposable SQL cache, queries, tree aggregation, report streaming |
| `src-tauri/src/diff.rs` | Test-only in-memory reference implementation; not the production comparison path |
| `src-tauri/src/commands.rs` | Tauri commands, task lifecycle, locks, caches, commit boundary |
| `src-tauri/src/sql_diff_tests.rs` | SQL/reference equivalence and comparison regression tests |
| `src-tauri/src/validation.rs` | Filesystem integration tests and opt-in resource stress tests |
| `src-tauri/src/storage_benchmark.rs` | Opt-in synthetic storage/query benchmark, included by the validation module |
| `src-tauri/tauri.conf.json` | Desktop identity, static assets, window settings, installer configuration |
| `src-tauri/capabilities/default.json` | Main-window Tauri capabilities |
| `tests/frontend.test.cjs` | Node test runner and JSDOM interaction regressions with a mocked Tauri bridge |
| `tests/preview.cjs`, `tests/preview-fixture.js` | Development-only localhost UI preview with labeled demonstration data |
| `docs/validation-0.2.1.md`, `docs/validation-0.2.2.md` | Historical validation evidence and performance limitations |

`target/`, `node_modules/`, and `src-tauri/gen/` are generated and ignored. Do not use generated files as the primary place to implement a feature. The production frontend asset directory is `frontend/`; do not place test fixtures, temporary logs, databases, or exported snapshots there.

## Directory, state, and history semantics

### Identity

- A homepage card represents a directory, not an individual snapshot.
- The displayed name comes from the folder name. Creating a directory record does not require a user-supplied snapshot name.
- Directory IDs and state IDs distinguish records internally. The same folder name under different paths is allowed.
- `storage::root_key` supplies path-based identity. It canonicalizes accessible paths, normalizes separators and Windows extended path prefixes, and lowercases on Windows. The database has a unique root-key constraint.
- Do not replace backend identity checks with a display-name comparison or a frontend-only string check. Offline paths cannot always be canonicalized; preserve the existing fallback and test any proposed changes.

### Manual operations

| Operation | Scan directory? | Save a history node? | Comparison/baseline behavior |
| --- | --- | --- | --- |
| Create | Yes | First node | Reject an already managed root; create the directory only on successful commit |
| Check changes | Yes | No | Replace current scan state and compare against the selected default historical baseline |
| Update snapshot | Yes | Yes | Append a node, set the new default baseline, and preserve the comparison against the preceding latest history for review |
| Compare saved states | No | No | Compare selected sides in the chosen left-to-right direction |
| View saved current state | No | No | Show the last successful scan with its timestamp; never imply live state |

In code, scan modes are `create`, `check`, and `capture`. A historical node's recorded entries and identity are immutable, although its remark can be edited and the node can be deleted.

Do not conflate the selected comparison's left side, the directory's persisted default baseline, its newest saved node, and its current scan state. After an update, the displayed before/after comparison intentionally differs from the newly selected default baseline. Preserve the explanatory notice.

Historical comparison works while the source directory is offline. Checking and updating require access to the source path. Reject cross-directory historical comparisons and selecting the same historical node on both sides. A reverse-time comparison is valid but must retain its direction and display the existing chronology warning.

### History management

- Retention `0` means unlimited; positive values keep the latest N saved nodes. The UI offers all, latest, and a custom count.
- Preview nodes that would be removed before applying a retention change. Use the existing confirmation flow for deletion.
- During a capture, apply retention only after a complete state has been saved within the successful transaction. Failure or cancellation must not prune history.
- Removing every historical node keeps the directory record. A later update can create its first history node again.
- Activity is aggregated by local scan date over the displayed 120-day interval. It measures differences observed by scans, not when files actually changed. Repeated checks can count the same differences repeatedly.
- Do not invent activity for imported history or label an unobserved day as a verified day with no changes.
- Focus patterns annotate and filter results; they are not watchers. Updating focus alone must not trigger retention cleanup or a scan.

## Scan lifecycle and filesystem correctness

The target workload includes large directories and individual large files. Maintain UI responsiveness and the successful-or-nothing storage boundary.

- Run blocking enumeration, file I/O, hashing, and database work through `tauri::async_runtime::spawn_blocking` or an equivalent established background boundary. An `async` declaration alone does not make blocking work safe.
- Permit only one active scan. Its frontend-generated UUID and backend cancellation token define the task identity.
- Emit `scan-progress` with `task_id`; frontend handlers ignore events for any other task.
- Preserve progress for enumeration, hashing/scanning, comparison, saving, and completion. Include total byte progress and the current file's hashing progress.
- Hash every included regular file. Equal size and modification time do not establish equal content; do not silently add a metadata-only shortcut.
- The scanner reads in 256 KiB chunks and checks cancellation between reads. A blocked OS read cannot be forcibly interrupted by this token; do not promise instantaneous cancellation under every filesystem condition.
- Retain the before/after metadata checks and one retry for a changing file. Persistent changes, unreadable files, inaccessible paths, and unsupported entries must fail the scan rather than silently producing a complete-looking partial snapshot.
- Enumerate directories, including empty directories. Record relative paths using `/` and retain Unicode rather than silently replacing names.
- Do not follow symbolic links, junctions, or Windows reparse points during traversal. Their recorded MD5 describes the link target path, not target file contents. Do not persist the target path text in place of its hash.
- A scan is not an atomic filesystem snapshot and does not lock source files. Avoid stronger consistency claims than the implementation provides.
- Complete scanning and its comparison before committing. Directory/state entries, references, activity, and retention changes belong in the transaction managed by `Store::commit_scan`.
- Cancellation is accepted before the saving boundary. Once saving begins, the UI disables cancellation and explains that the operation is completing.
- Release task ownership on success, error, or cancellation. Do not leave creation/update disabled indefinitely.
- Preserve frontend request counters for navigation, comparison, pagination, and drag validation; late responses must not overwrite a newer user action.

## Ignore rules and comparison semantics

### Ignore scope

The root `.snapshotignore` is read first, then extra directory rules. Later matching rules win. The matcher supports comments, blank lines, globs, trailing-slash directory rules, leading-slash root anchoring, and `!` exceptions. It is not a complete implementation of Git ignore semantics and does not read ignore files from every child directory.

The root ignore file itself is excluded. Preserve `has_negations` behavior: blindly pruning an ignored subtree can lose a later re-included entry.

Store the effective rules in each newly captured state. When rules differ, compare only entries included by both states' scopes; changing an ignore rule must not masquerade as content addition/deletion. Retain warnings for schema-1 history with unknown complete scope and for history without complete directory entries.

### Difference categories

| Status | Meaning |
| --- | --- |
| `added` | Relative path exists only on the new side within the comparable scope |
| `deleted` | Relative path exists only on the old side within the comparable scope |
| `modified` | Same path has a changed entry kind or MD5 |
| `moved` | An added/deleted pair has a unique matching size and MD5; this is a suspected move/rename |
| `metadata` | Same path, kind, and hash, but relevant size or timestamp metadata changed |

Do not present suspected moves as proven operations. Ambiguous hash matches remain additions/deletions. Directory timestamps are not content modifications; changing a child should not flood the result with modified ancestor directories. Metadata-only changes remain hidden by default.

`sql_diff.rs` is the production implementation. It uses SQL `EXCEPT` to find candidates and merges ordered streams rather than materializing both complete historical snapshots. Keep it semantically aligned with the test-only reference in `diff.rs` whenever classifications or scope rules change.

## Persistence and exchange formats

### Persistent store

The application resolves its data directory using Tauri's path API and stores `snapdiff.sqlite3` there. The production identifier is `com.snapdiff.desktop`. Do not derive application paths from the process working directory or assume app data and app configuration directories are interchangeable on every platform.

The persistent store uses SQLite WAL, foreign keys, a busy timeout, and a mutex-protected connection. Its primary tables are:

- `directories`: path identity and serialized directory configuration.
- `states`: saved/current state identity and compact headers.
- `entries`: state-scoped relative paths, kinds, sizes, times, and hashes.
- `scan_events`: successful scan activity.
- `migrated_json`: legacy migration markers.

Do not weaken synchronization or journaling for the persistent database to improve a benchmark. Relaxed settings in disposable comparison caches do not justify changing persistent durability.

### Versions and encoding

- Database version: `entry_codec::DATABASE_VERSION`, currently 3, tracked with SQLite `user_version`.
- Public snapshot JSON version: `models::CURRENT_SCHEMA_VERSION`, currently 2.
- Archive envelope: `format: "snapdiff-archive"`, `schema_version: 2`.
- Application release version is independent of all three formats.

Entries use `PRIMARY KEY(state_id, path) WITHOUT ROWID`. Non-directory MD5 values occupy 16-byte BLOBs internally and serialize as lowercase hexadecimal strings externally. Directory entries have no content hash. Modification times retain integer seconds and nanoseconds; do not round them through UI display strings. SQLite individual file sizes must fit its signed integer range, while difference sums use wider arithmetic where required. Preserve overflow checks and the custom aggregation rather than blindly replacing them with SQLite `SUM`.

Database schema changes require explicit version handling and transactional migration, or an explicitly user-authorized reset of disposable data. The project is still in testing and storage rules may change substantially, but that is not standing permission to delete a user's existing data. Reject unsupported newer database formats instead of recreating the database.

### Legacy migration, imports, and exports

- Legacy `snapshots/*.json` files migrate on startup with per-file markers. Preserve the original files and prevent deleted migrated history from reappearing on restart.
- Malformed legacy files produce warnings while valid files can still migrate. Never silently discard failures.
- Validate imported format versions, duplicate paths, safe relative paths, entry kinds, hashes, IDs, and rules through the storage layer. Do not trust frontend input or imported names as executable content.
- Import supports archives and individual legacy snapshot JSON. A duplicate compatible state ID is skipped; a conflicting state ID must not overwrite local recorded entries.
- `preview_import` parses and retains the exact archive behind a ticket. `import_snapshots` commits that ticket's data, not a newly reread file that might have changed after the preview.
- Import must not fabricate current scan state/activity or immediately prune imported history to satisfy a retention setting.
- Export saved history as metadata-only JSON. Current transient scan state is not automatically another historical node.
- Stream archive entries and report output through the existing temporary-file/atomic-replacement helper. Leave the previous target intact on failure and clean up only the operation's own temporary files.
- A report preview is bounded to 64 KiB. Full report export and copying all filtered paths cover the whole matching result, not only the visible page.

## Comparison cache and performance

Comparison results live in independent disposable SQLite databases, not the persistent history tables. Closing the cache connection cleans up its temporary database. This lets a displayed result remain available after historical retention removes a source node.

The backend cache retains at most eight comparisons and evicts older results when the accumulated row count exceeds 250,000, while retaining at least the latest result. Handle expired comparison IDs by reporting the condition and recomputing on request.

Filtering, searching, sorting, pagination, and directory aggregation belong to the backend. A result page contains at most 100 items. Do not ship a complete directory or complete difference array to JavaScript merely to implement a filter. Reuse tree aggregation for identical filters; invalidate it after focus changes.

Current limitations to remember when proposing optimizations:

- Scanning still holds its collected paths and resulting metadata before commit.
- Import preview still retains the parsed archive in memory.
- Cross-history entry deduplication is not implemented.
- Persistent-store operations share one connection and mutex.
- Building a disk cache can be slower than an in-memory diff when most entries changed.
- Strict MD5 scanning must read the included content, even though content is never stored.

Benchmark the actual changed phase and describe time/memory tradeoffs. Do not extrapolate a synthetic metadata benchmark to a 200 GiB filesystem scan, or claim unlimited scale or constant memory.

## Frontend and Tauri interface conventions

Frontend modules share `window.SD` and load in order: `core.js`, `results.js`, `history.js`, `app.js`. Initialization is idempotent. Use existing dialog, action, escaping, notice, clipboard, and busy-state helpers instead of adding competing global state systems.

Tauri commands are registered in `src-tauri/src/lib.rs` and implemented in `commands.rs`. JavaScript invoke names are snake_case; argument objects use camelCase names such as `directoryId`, `taskId`, and `comparisonId`. Most returned Rust models use snake_case fields; `ScanRequest` and `DiffQuery` explicitly use camelCase serialization. Check the actual serde declarations rather than applying a blanket naming conversion.

| Area | Commands |
| --- | --- |
| Directory overview | `list_directories`, `get_directory`, `validate_directory_path`, `get_storage_info` |
| Manual scan | `scan_directory`, `cancel_scan` |
| History and activity | `list_history`, `select_baseline`, `get_activity`, `set_remark`, `delete_nodes` |
| Settings | `preview_retention`, `update_settings`, `set_focus_patterns`, `delete_directory` |
| Comparison/results | `compare_states`, `query_diff`, `get_report`, `export_report`, `get_diff_paths` |
| Metadata exchange | `export_snapshots`, `preview_import`, `import_snapshots` |

For an interface change, update Rust arguments/models, registration, frontend callers, test mocks, and documentation together. Keep read/query commands separate from mutations. Validate mutations on the backend even when the UI disables their buttons.

Use `textContent` for untrusted text and `SD.esc` wherever values enter generated HTML, attributes, or data attributes. Paths, remarks, imported names, and error strings are untrusted data. Preserve parameterized SQL and internally controlled sort/table names. Do not build SQL identifiers or executable shell strings from user paths.

Keep Tauri capabilities narrowly scoped to the required UI. The existing configuration has `core:default`, `dialog:default`, and `csp: null`; these are current settings, not permission to add remote content, broadly exposed filesystem APIs, shell plugins, or more capabilities without a concrete need.

### UI behavior that must remain intact

- The homepage exposes creation and per-directory comparison/update actions; do not reintroduce a redundant global comparison button.
- Creation has a directory path and optional extra ignore rules, with no required snapshot-name field.
- While scanning, close the creation form, hide/disable its submit action, disable competing mutations, and preserve a working cancellation control before commit.
- Native `tauri://drag-drop` events provide folder paths. Accept one accessible, unmanaged folder on the idle homepage; validate it in Rust before opening the prefilled creation dialog.
- A drop never submits the form or starts hashing. Leave the dialog open for ignore-rule configuration.
- Files and multiple dropped paths are rejected with a message. Do not replace a form already open, interrupt a scan, or open creation from a directory workspace.
- Preserve the drop request counter and eligibility recheck after validation so a delayed result cannot replace a newer drop or manual configuration.
- Suppress the page's default `contextmenu` event globally, including inputs and keyboard-triggered menus. Prevent browser navigation caused by DOM file drops. Native OS title-bar and file-picker menus are separate from page menus.
- Retain native directory/save/open dialogs and accessible modal behavior. Ensure history content can scroll without hiding its action buttons at the minimum window size.

### Startup and window state

Startup ordering is an explicit requirement: never display a default-size window and resize or move it afterward to restore saved geometry.

- Keep the configured main window initially hidden with background `#0d1219`.
- Keep critical dark first-frame CSS in `index.html`, not only an external stylesheet.
- Register the official window-state plugin before the startup-window plugin. It restores size, position, and maximized state in its window-ready callback.
- Exclude the window-state `VISIBLE` flag. Only the startup readiness gate may reveal the window.
- `startup_window::Readiness` requires both geometry readiness and `PageLoadEvent::Finished`, regardless of callback order, and reveals the main window once.
- The plugin saves state in `.window-state.json` under the app configuration directory. Do not couple window layout to snapshot history.
- Restore normal dimensions beneath maximization. Do not restore a minimized/hidden state that makes a new launch appear missing. Saved offscreen positions should fall back to an available screen through the existing plugin behavior.
- The frontend startup overlay remains until initialization completes. Remove `inert` and clear `aria-busy` on success or failure so a startup error is visible and retryable.
- Do not add artificial loading delays. Preserve the reduced-motion handling for the spinner.
- Retain the Windows GUI subsystem attribute in `main.rs`, including development builds; the app must not spawn a console window.

## Build and validation

Run commands from the repository root unless specified otherwise. Windows development requires Rust, Node.js, the Microsoft C++ toolchain/Windows SDK, and WebView2 prerequisites for Tauri. Use existing dependencies and lockfiles; prefer `npm ci` for a reproducible clean install.

```powershell
npm ci
npm run tauri dev
```

There is no `npm run build` script. Tauri embeds `frontend/` directly.

### Choose checks appropriate to the change

For behavioral changes, run the affected tests and relevant checks. For changes spanning Rust and the frontend, the standard set is:

```powershell
cargo test --lib
npm test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
git -c core.autocrlf=false diff --check
```

Run `cargo fmt --all` when Rust formatting needs correction, then inspect the diff. Avoid unrelated line-ending churn. Documentation-only changes normally need content/link/format verification, not a new application build or redundant regression run.

As of the 0.2.5 work, the regular suite passed 35 Rust tests and 17 frontend tests; three stress tests were ignored by default. Counts are historical reference points, not a substitute for running tests after behavior changes.

Frontend tests use a mocked Tauri bridge and JSDOM. They verify state transitions and request contracts but cannot prove Windows native drag/drop, WebView rendering timing, clipboard permissions, or installer behavior. Cover meaningful regressions, especially cancellation, stale async responses, scope changes, migration rollback, and user-confirmed mutations. Avoid tests that only restate markup for a trivial cosmetic edit.

### UI and native verification

For a browser layout preview:

```powershell
node tests/preview.cjs
```

Open `http://127.0.0.1:47831`. The preview uses labeled fixtures and does not scan real directories. It is not a native acceptance environment. Stop the temporary server when finished.

For native acceptance, create a temporary Tauri configuration under `target/` that changes the application identifier and title. If overriding `app.windows`, preserve the main label, hidden/dark startup, minimum dimensions, and native drag/drop configuration. Build and launch that isolated executable against generated test folders:

```powershell
npm run tauri build -- --no-bundle --config target/acceptance/tauri.json
```

The overlay file must exist first; choose an independent identifier such as `com.snapdiff.acceptance.<test-name>`. The resulting executable still uses the common `target/release` location, so preserve a separate copy before a later production build if needed. Do not mistake the overlay executable for a production release or attach to a user's running instance just because its title contains SnapDiff.

For startup/window work, verify normal move/resize, close/reopen, maximized close/reopen, restoring normal geometry, and invalid saved positions. For drop work, verify an actual Explorer-to-app folder drop, a prefilled form with no scan, editable ignore rules, rejection cases, and right-click behavior in both page and inputs.

Known verification gap: at the end of 0.2.5, homepage right-click suppression was checked natively, while cross-window folder dropping was covered by mocked frontend and backend validation tests but could not be completed through the available UI automation tool. Do not describe that native drag/drop flow as already verified. Installer creation also does not establish successful install/upgrade/uninstall behavior. Record newly completed acceptance evidence when those gaps are resolved.

### Resource-intensive tests

Stress tests are opt-in and create real files or large synthetic data. Do not run every ignored test automatically for a routine UI change. Select a workload fitting the change and available disk/time resources, preferably one at a time:

```powershell
cargo test --release --lib stress_storage_queries -- --ignored --nocapture --test-threads=1
cargo test --release --lib validation::stress_many_files_and_history -- --ignored --nocapture --test-threads=1
cargo test --release --lib validation::stress_large_file -- --ignored --nocapture --test-threads=1
```

Configuration variables and defaults:

| Variable | Default | Current upper bound | Workload |
| --- | --- | --- | --- |
| `SNAPDIFF_STORAGE_FILES` | 100,000 | 1,000,000 | Synthetic metadata storage/comparison benchmark |
| `SNAPDIFF_STRESS_FILES` | 20,000 | 200,000 | Real small-file scan/history test |
| `SNAPDIFF_STRESS_GIB` | 1 | 200 | Real large-file write and MD5 scan |

These tests use `tempfile` data and should never target a user's game or work directory. Restore any environment variables changed for a test. Published timings must include workload, hardware/context, included phases, and limitations. See the existing validation documents for historical measurements; do not report them as fresh results.

## Releases and completion reporting

When a release bump is warranted, keep these locations consistent:

1. `package.json`.
2. Root and root-package versions in `package-lock.json`.
3. `src-tauri/Cargo.toml`.
4. The `snapdiff` package entry in root `Cargo.lock` (normally updated by Cargo).
5. `src-tauri/tauri.conf.json`.
6. The visible version badge in `frontend/index.html`.

Do not globally replace unrelated dependency versions in `Cargo.lock`. Do not bump database/JSON formats merely because the release version changes. A documentation-only addition does not require a version bump.

Build production installers without an acceptance overlay:

```powershell
npm run tauri build
```

With the current configuration, output normally includes:

```text
target/release/snapdiff.exe
target/release/bundle/nsis/SnapDiff_<version>_x64-setup.exe
target/release/bundle/msi/SnapDiff_<version>_x64_en-US.msi
```

Confirm build completion and that the expected artifact/version exists before linking it. For changes affecting native startup or packaging, inspect executable version resources and preserve the Windows GUI subsystem. Do not install, publish, or claim upgrade acceptance just because bundling succeeded.

Finish with the implemented behavior, relevant checks and their results, and any material unverified behavior. Clearly distinguish code implementation, mocked tests, real filesystem tests, native UI acceptance, and installer generation. Never claim a user-visible operation was verified when a tool failed before reaching it. Use absolute file links when the host application requires them.
