# Local source protocol 1

Maintained fork: `https://github.com/audiodude/funes`. AI-assisted implementation by OpenAI Codex. This interface is separate from ranked search and MCP; it never accesses a remote memory or stores raw transcripts. The existing search index and bridge remain independently usable.

## Invocation and scope

`funes source` reads one JSON request from stdin and writes one JSON response to stdout. No source text or paths are logged to stderr. Requests are limited to 64 KiB. Every request has `protocol: 1`, `op`, `corpus` (an absolute local directory) and `scope` (an absolute local enrollment JSON file). Remote selectors, relative paths and symlink escapes are rejected. No environment-default corpus or roots are used. Enrollment file schema: `{"version":1,"roots":{"claude":["/absolute/root"],"codex":["/absolute/root"],"omp":["/absolute/root"]}}`. All three keys are required, empty arrays explicitly enroll no sources for that harness. Files and metadata database are private (0600), directories 0700. Enrollment is independently managed, not created or modified by Actomasto. Scope identity is SHA256 of canonical parsed enrollment; changes invalidate old cursors.

Successful responses: `{"protocol":1,"ok":true,"result":{...}}`. Errors: `{"protocol":1,"ok":false,"error":{"code":"..."}}`. Error codes are content-free. Exit 0 on success, nonzero on error. Unknown operations/protocol/fields fail closed. A consumer must validate every response before using it.

## Operations

- `capabilities`: result contains `protocol:1`, `build_revision` (full committed fork SHA), `identity:"actomasto-v1"`, `harnesses:{"claude":"claude-schema2","codex":"codex-0.144.1-schema1","omp":"omp-session3-schema1"}`, `local_only:true`, `metadata_only:true`, `revision_bound:true`, `snapshot_enumeration:true`, `coverage_freshness:true`. Does not open originals.
- `refresh`: independent operator/indexer request. Discovers all enrolled *.jsonl sources, including empty/malformed/unsupported/unsearchable sources, fingerprints originals and records a metadata-only immutable inventory snapshot. Does not require embeddings or an OMP session. Result contains `snapshot`, `sources`, `scope_id`. This is NOT an Actomasto operation. Repeated periodic refresh is the independent source-index lifecycle; semantic indexing may run separately.
- `enumerate`: optional `cursor` (null or opaque string), `limit` (1..128; default 64), optional `harness` (claude/codex/omp). Result: `snapshot`, `scope_id`, `sources:[{id,harness,revision,state}]`, `next_cursor` (null when complete), `coverage` (`current`, `lagging`, or `unavailable`), `refreshed_at`, and `lag_seconds`. `state` is `present` or `missing`. IDs are opaque SHA256 values, no paths or prompts. Snapshots are stable across pages; late discovery and source mutations appear on subsequent refreshed snapshots regardless of session dates/search chunks. Cursor expiry or scope/index rebuild returns `invalid_cursor`; restart at null with durable unit deduplication, never skip to present. No refreshed inventory returns `coverage_unavailable`.
- `turns`: `source_id`, `revision`, optional `offset` (nonnegative integer, default 0), `limit` (1..128). Reads source metadata and returns complete normalized turn metadata, not original text: `turns`, `next_offset` (null when done), `status`, `pending_turns`. Every turn has `ordinal`, `id`, `session_id`, `user_id`, `start`, `end`, `invalid`, `boundaries:[{cwd,time,end,invalid}]`, `items:[{id,message_id,role,time}]`, `message_ids`, `bytes`. `role` is `user` or `assistant`; timestamps are epoch seconds or null (not inferred), end is null if absent. `bytes` measures the full ordinary text after harness framing removal, before policy filtering. Boundary list includes all project/time-relevant excluded/control records along the turn lineage, including the next user boundary when that completes a previous turn. Status distinguishes `complete`, `incomplete_turn`, `incomplete_write`, `deferred_future`, `malformed_record`, `oversized_source`, `unsupported_version`, and other typed schema/lineage errors. Completed prefix turns are retained for per-stream malformed/oversize outcomes. Unknown schema/version/provenance/lineage pauses that harness. EOF is never a completion signal. A metadata request may transiently parse original records in Funes but cannot return content.
- `read`: `source_id`, `revision`, `ordinal`. Returns `{turn: <same turn metadata with each item additionally containing text>}`. Returns the complete ordinary text, never reasoning/tool payloads or search excerpts. Must validate the source file and dependency inputs before and after processing; no mixed revisions. No durable raw-source cache. Maximum ordinary turn size 32 MiB, maximum complete response 256 MiB; oversized turns are terminal metadata outcomes, never excerpts. Temporary buffers only.

`source_changed`, `source_missing`, `source_unavailable`, `invalid_cursor`, `coverage_unavailable`, `unsupported_protocol`, `invalid_scope`, `invalid_request`, `oversized_response` distinguish failures; never substitute search text. Source revision hashes complete original bytes and file identity/size/mtime/ctime so rewrites, rotation, truncation and partial writes cannot alias. Inventory snapshot metadata contains private locators; response metadata does not expose them. Pagination is bounded and an expired cursor requires explicit replay with stable IDs.

### Resource bounds

Every request scans its revision-bound original in bounded records; turn pagination limits materialized output, not the discoverable prefix. Temporary private SQLite stores only identity, cwd, time, lineage, offsets and hashes. It never stores ordinary text, tool payloads, reasoning, or raw JSON; original ordinary text is reread from the pinned descriptor for the requested turn and checked against its record digest. Scratch metadata is removed at request completion. RAM is bounded independently of transcript length; scratch disk and scan time scale with metadata volume. Actual resource exhaustion is a retryable `source_capacity` error. A selected metadata turn/page beyond the 256 MiB response budget is `oversized_response`, not a terminal ordinary-text exclusion. Original records and ordinary turns retain their existing 32 MiB limits. Inventory retains 32 immutable snapshots; older cursors explicitly expire.

Coverage reports `refreshed_at` (epoch seconds) and `lag_seconds` for the latest independently refreshed inventory. More than 300 seconds without refresh, or a backwards wall clock, reports `coverage:"lagging"` rather than claiming no activity. Source capabilities explicitly include `coverage_freshness:true`. Consumers pause affected conversation work on lag while Git continues. Refresh at least once per minute for normal operation; no active agent session is required.

An enrolled root that is missing or unreadable reports `coverage:"unavailable"` for its harness, not successful empty activity; restoring the root and independently refreshing recovers it. Empty enrollment arrays intentionally enroll nothing. Missing originals remain visible as tombstones until restored or an independently rebuilt corpus replaces the inventory; rebuilding never resets Actomasto's own terminal markers.

## Legacy identity and normalization

Unit ID is SHA256 of UTF-8 `client:session:user-message-id`; item ID is SHA256 of UTF-8 `client:session:message-id`. Native identifiers and session IDs are original strings. Without a native ID, use SHA256 of UTF-8 `session:turn-context:record-index:raw-record-digest`; raw-record-digest is SHA256 of UTF-8 lowercase hex of original bytes INCLUDING newline. Record index is zero-based, counting complete parsed records, parser context after the record; absent turn-context is empty string. Never normalize JSON before hashing. Metadata preserves native/fallback message IDs for legacy adapter-unit marker continuity. Source IDs/revisions are independent and never used as conversation identities.

Each ordinary item also carries `identity:{native_id,record_index,turn_context,raw_record_digest}`. `native_id` preserves the original JSON identifier or null; `record_index` is its original zero-based position; `turn_context` is the parser context after that record; `raw_record_digest` is the SHA256 of original lowercase hex bytes including newline. These are metadata, not source text. Actomasto checks that these inputs reproduce each message identity before accepting the turn. Missing session identity remains invalid metadata; its legacy hash input is the literal Python spelling `None`, not JSON `null`.

Harness mappings are the existing Actomasto `claude-schema2`, `codex-0.144.1-schema1`, `omp-session3-schema1` contracts: Claude positive origin human/typed provenance and linked parents with end_turn completion; Codex canonical response_item user confirmed by matching user_message or native UserMessage completion plus exact active turn association and task_complete; OMP v3 explicit user attribution, message timestamps (milliseconds), linked parents and stop with completedAt. Framing/injected records, unknown authorship, reset/compaction/synthetic boundaries and excluded tool time/cwd boundaries retain existing exclusions. Linked shared ancestors are not emitted twice by consumers. Missing metadata remains invalid, never inferred from text. Future times remain retryable, not terminal.

The Codex capability label preserves the identity contract, not a single CLI
version: observed versions `0.142.5`, `0.144.1`, and `0.154.0-alpha.6.2` are
supported. Ordinal-bearing records and native `item_completed` / `UserMessage`
confirmations are accepted only with matching session, active turn, and exact
raw text. CLI and VSCode sessions require `thread_source: "user"`; unknown
provenance facets still fail closed. Image-bearing messages whose generated
canonical wrappers do not exactly match the user confirmation remain excluded;
matching text suffixes are not sufficient.

OMP child `session_init` and agent-attributed prompts are provenance barriers,
not human input. Known usage/recovery metadata and excluded `bashExecution` /
`fileMention` records no longer reject an otherwise supported stream; excluded
records still contribute lineage and time boundaries. Unknown schemas and
missing originals remain errors. No enrollment, tombstones, or durable identities
are changed by this compatibility repair.

Current compatibility also includes Claude `2.1.280`, Codex `ImageView`
completion records, and client-authored function-call-output bookkeeping
(`client_authored` and `fallback_token_limit_override`). That bookkeeping is
accepted only on tool outputs, never on user/assistant messages. OMP accepts
string `upstreamModel` and unsigned-integer `credentialId` metadata. These fields
are excluded from evidence; unknown fields, malformed types, attribution rules,
and completion gates still fail closed. This does not restore deleted originals
or clear inventory tombstones.

OMP 18.3.1 assistant `requestControls` records are provider replay bookkeeping,
not conversation evidence. The accepted shape is an unsigned `messageIndex`,
optional `tools` with string-array `declared`/`deferred`/`active` fields, and
optional `effort` with `topLevel`/`tail` values of null or
`low`/`medium`/`high`/`xhigh`/`max`. Unknown fields, malformed values, missing
required nested fields, and non-assistant carriers remain rejected. Completion,
human attribution, lineage, and project/time boundary gates are unchanged.

OMP user messages also accept boolean `liveSteered` display metadata. It does
not establish human attribution or complete an unfinished turn; malformed values
and non-user carriers remain errors. `mode_change` settings records accept a
string `mode` and optional object `data`, with native ID/parent lineage. They
preserve time and lineage boundaries without resetting the conversation or
exposing mode-specific data. Unknown top-level fields still fail closed.

The October 2 steering/mode compatibility repair was exercised through the real
`source` CLI with synthetic originals and read-only existing-session checks.
Across one 657-source inventory, all 43 sources rejected by the deployed reader
for `unknown_content_schema` became readable: 36 complete and seven with pending
turns still excluded. Three concurrently changed originals remained rejected by
the revision guard. No live enrollment, source inventory, or service configuration
was changed by these checks. Implementation and verification assisted by OpenAI Codex.

## Consumer durability and permissions

Actomasto checks gates before all requests and before durable updates. It maps every cwd boundary to the unique deepest discovered repository, including ineligible nested repositories; applies whole interval and per-message eligibility; checks existing source_markers and `adapter-unit:*` markers before read; applies whole-turn secret/blocklist filters before persistence or transmission. It records a terminal marker only after durable exclusion or enqueue. Enumeration cursor advances only after all source turns are durably handled; pending or unavailable activity is revisited on subsequent passes without changing collection/expiry times. Full metadata re-enumeration is safe with exact unit deduplication. No adapter stream offsets/inodes are converted into cursors. Configuration/database migration preserves old terminal markers and pending timestamps, initializes conversation health closed, and rejects old binaries via database/config versioning.

Off/login/budget/revoke controls govern Actomasto source use, not independent indexing. Actomasto purge affects neither original files nor the independent Funes corpus. Generation remains hosted and separately consented. No live memory installation, enrollment, indexing, migration or deployment is part of implementation verification.

## Verification

Initial integration runtime: `69387f12dca29c2c8e939b0d9890e5768cc2067c`. [Actomasto evidence](https://github.com/audiodude/actomasto/blob/feat/funes-source/verification/funes-integration.json) records 210 passing tests against that executable, 32 exact legacy-equivalence cases, real CLI migration/reopen checks, daemon outage/recovery, and a separately authorized synthetic Anthropic run. [Bridge evidence](https://github.com/audiodude/omp-funes-bridge/blob/feat/funes-source/verification/fork-source.json) records its fresh pinned checkout build, repeatable installation/removal, indexing catch-up, and native MCP live-reader retrieval. These are historical records, not evidence for later dependency builds.

Re-run source regressions with `cargo test --locked --lib --bin funes --test local_source_normalize`; consumer regressions require `FUNES_TEST_BIN=/absolute/fork/funes uv run pytest -q` in Actomasto. All source fixtures are authored synthetic data. No live-memory installation, enrollment, deployment, or posting was performed.

The September 2026 dependency refresh uses stable `hf-hub` 1.0.0, updates compatible locked dependencies, and retains Lance 11.0.0 and source protocol 1. Actomasto's 210 tests and native OMP 18.1.17 completed/aborted-turn consumption passed against the refreshed build with synthetic originals; completed turns preserved text and provenance, aborted turns remained pending, and restart and eligibility checks emitted no replay or excluded units. Consumers must pin the committed build revision they verify. Local-only commits must be built from the supplied worktree with `cargo build --locked`; they cannot be fetched from the public fork until separately published. No live Hub publication or hosted generation was exercised for this refresh.

### September 16, 2026 compatible dependency refresh

The `update-local-20260916` branch starts from maintained-fork `origin/main` at
`e1e398c`; fetching and pulling main required no source changes. `cargo update`
refreshed 25 locked packages within the existing manifest constraints, including
Clap 4.6.7, Rustls 0.23.45, and Quinn 0.11.12, and removed `tinyvec_macros`.
This includes the six lockfile updates from `update-local-20260912`; no parser
changes or unrelated upstream features were imported. Lance remains pinned to
11.0.0, `hf-hub` remains 1.0.0, and local source protocol 1 and `actomasto-v1`
capabilities are unchanged.

Dependency resolution succeeded; builds, linting, and tests are deferred to the
local rollout and the earlier verification results above do not cover this
lockfile. Build the committed revision with `cargo build --locked`, then run the
source regressions and consumer checks described above before activating it.
GitHub source publication does not publish a release or a Hugging Face artifact;
this refresh does not authorize either, and creates no release tag.

### September 17, 2026 upstream and dependency refresh

The `update-local-20260917` branch starts from maintained-fork `origin/main` at
`e1e398c` and preserves the completed `update-local-20260916-evening` branch at
`6a96776`. Pulling origin main required no changes. Upstream main through
`0612fce1a05450366bf2160f3dae5dcfcd3d48ce` adds the Linux BLAS exponential
underflow fix, boundary/softmax regressions, and a five-iteration ragged-batch
backend benchmark. Masked attention weights now underflow to zero instead of
leaving tiny weights that can cause subnormal arithmetic stalls. The only merge
conflicts were the package version in the manifest and lockfile; the fork keeps
`1.4.0+dev` rather than upstream's `1.3.1+dev`.

`cargo update` advanced `unicode-ident` from 1.0.25 to 1.0.26 within the existing
manifest constraints. Lance stays pinned to 11.0.0 and `hf-hub` remains 1.0.0.
OMP parsing, local source protocol 1, and `actomasto-v1` capabilities are unchanged.
The inference change affects extremely small exponential results on Linux;
upstream's added regressions and consumer checks still need to run against this
committed revision before rollout.

Dependency resolution succeeded. All builds, tests, formatting, linting, and
runtime checks are explicitly deferred to the integration owner; historical
results above do not verify this revision. Use system `protoc`, run
`cargo build --locked`, then the source and consumer regressions described above.
The source regression command includes the new BLAS unit tests. No local service
or configuration was changed, and no release tag, deployment, or Hugging Face
publication is part of this refresh.

### September 21, 2026 upstream and dependency refresh

The `update-local-20260921` branch starts from maintained-fork `origin/main` at
`94edc7b`; pulling main required no changes. Upstream
`https://github.com/huggingface/funes` is merged through
`f120a742ce485668e0ca2744cc1d3819564d5c43`. This adds the
[`.funes.jsonl` input format](funes-jsonl.md), `index --check`, arbitrary harness
facets in recall, per-turn cwd/repo resolution, duplicate-chunk suppression within
one append, and release-workflow hardening. The fork retains version `1.4.0+dev`,
its OMP indexing budgets and scanner receipts, revision-bound local source
protocol 1, and the existing Codex/OMP provenance compatibility fixes. The new
serialized turn format is independent of the local source protocol; it does not
enroll third-party exports as Actomasto originals.

OMP turns now carry their recorded cwd through the shared turn model, preserving
repository facets after the upstream indexing refactor. `index --check` also
supports explicit OMP input without writing graph sidecars; incomplete OMP
coverage rejects the checked unit rather than reporting a clean partial parse.
The existing indexing path still persists its graph metadata.

`cargo update` refreshed seven packages within the existing manifest constraints:
`cc` 1.4.6 → 1.4.7, `find-msvc-tools` 0.1.12 → 0.1.13,
`generator` 0.8.9 → 0.8.10, `hyper-rustls` 0.27.9 → 0.27.10,
`libredox` 0.1.24 → 0.1.25, `rand` 0.10.2 → 0.10.3, and
`stop-words` 0.10.0 → 0.10.1. Lance remains pinned to 11.0.0 and `hf-hub`
remains 1.0.0.

Revision `0c443bf8d22ec0a0a1c731681b8efefdeeb3e189` was built with system
`protoc`, the default BLAS backend, an optimized dev profile (opt-level 1,
debug info and incremental compilation disabled), and lld. Its exact build
revision passed the bridge capability gate. Reusing a target cache still requires
rebuilding and checking `source` capabilities; source commits are not binaries.

Verification passed 314 tests across the library, binary, `local_source_normalize`,
`funes_jsonl_index`, `index_check`, `index_recall`, and `reindex_incremental`.
Direct CLI checks accepted complete OMP input, rejected malformed input, and
left a disposable Funes home empty. Normal OMP indexing produced usable receipts.
Actomasto passed 221 tests against this binary. Native OMP 18.2.8 completed and
aborted turns passed consumer checks, and the bridge passed installation,
indexing, native MCP recall/get, and warm-reader refresh probes.
Formatting and linting were not rerun. The removed Hugging Face publication step
stays removed; only the source branch was pushed, without a release tag or artifact.
Verification assisted by OpenAI Codex.

### September 25, 2026 compatible dependency refresh

The `update-all-20260925` branch preserves `update-all-20260924` at `c6396271`
and the later current-schema normalization changes at `1c4e2fc2`. Both source
worktrees were clean. Fetching maintained-fork `origin/main` and upstream
`huggingface/funes` main required no merge: upstream remains at `8c7c5ca`
(after stable v1.3.3), already included in the fork.

`cargo update` refreshed twelve compatible locked packages, including
`encoding_rs` 0.8.42, `hyper-util` 0.1.21, `rustls-platform-verifier` 0.7.1,
`smallvec` 1.16.2, and the WebAssembly bindings, and removed the unused
`multiversion`/`multiversion-macros` 0.9.0 dependency pair. Lance remains pinned
to 11.0.0; the manifest, local source protocol 1, and `actomasto-v1` identity
remain unchanged. Current Claude, Codex, and OMP metadata normalization is
retained.

Verify the committed build with `cargo test --locked --lib --bin funes --test
local_source_normalize`, then exercise `source` capabilities and a synthetic
refresh/enumerate/turns/read round trip before consumer activation. Earlier
verification above does not cover this lockfile. This source refresh does not
switch installations or services, publish a release or Hugging Face artifact,
or upload memory.

### September 29, 2026 upstream integration

The `update-all-20260929` branch starts from the deployed custom revision
`c27917ac833c34b9f5b7f39e397efc56f6d59899`; maintained-fork `main` was clean
and already current. Upstream `huggingface/funes` main through `9712c6f` adds
external integration spools, rows-first indexing, refreshed search indexes,
streamed inference-weight loading, and pooled MCP reads.

The fork retains native OMP indexing behind an explicit local path and
`--harness omp`, including bounded embedding, committed coverage receipts,
scanner history, graph dependencies, full citations, and dry-run isolation.
Native OMP obligations survive the upstream retirement of other native indexing
sources. Claude, Codex, and OMP normalization through `funes source` remains
independent of integration spools; protocol 1, parser capability labels,
`actomasto-v1`, identity derivation, and fail-closed evidence gates are unchanged.

The regenerated compatible lockfile uses RMCP 3.5.0 (following upstream's
RMCP 3 migration), `serde_with` 3.24.0, `cc` 1.5.1, `zerocopy` 0.8.59, and
`tokio-rustls` 0.26.6. Lance remains pinned to 11.0.0, Arrow to the 58 series,
and `hf-hub` to 1.0.0. The obsolete YAML dependency and unused release-bucket
helper are removed; software updates still use the fork's GitHub Releases,
not a Hugging Face deployment.

Builds use the default BLAS backend and the existing optimized dev-profile
convention (opt-level 1, debug information and incremental compilation disabled,
lld). Consumers must check `source` capabilities against the exact committed
build revision before switching their pinned executable. No live Hugging Face
tests, uploads, or deployments are authorized by this refresh.

Verification of the integrated source passed formatting, warning-free Clippy for
both the default and ONNX backends, the offline installer regression, and
`cargo test --locked --all-targets -- --test-threads=1`: 347 reported passes,
including five live-Hub tests that returned early because `HF_FUNES_TEST_TOKEN`
was empty. No remote-memory or publication coverage is claimed.

Runtime probes exercised source capabilities and a synthetic
refresh/enumerate/turns/read round trip, bounded native indexing and resumed
coverage, semantic recall, and a real MCP initialize/tools-list/get exchange.
Native OMP 18.4.3 fixtures retained three parent/child/advisor sessions and nine
chunks while excluding reasoning, tool results, and control text. Its completed
lifecycle emitted one valid source turn; its aborted lifecycle stayed pending.
Live services are switched separately by the integrating operator.

### September 30, 2026 upstream and dependency refresh

The `update-all-20260930` branch preserves custom revision `eb7babe` and merges
upstream main through `86fb8e1`. Upstream now refreshes installed integrations
after a software update while retaining explicit-source installs and their
bindings. The fork still fetches software binaries only from GitHub Releases.
This maintenance operation does not run `funes update`, change installed
integrations, upload memory, or publish release artifacts.

The compatible Cargo refresh advances `async-compression` to 0.4.49 and
`compression-codecs` to 0.4.44. Lance, Arrow, and the source protocol retain their
existing constraints. Native OMP parsing, lineage/provenance, bounded indexing,
and source protocol 1 are preserved without parser changes. Build the committed
revision before consumer verification; activation remains a separate operator
step using the unchanged enrollment and corpus.

### October 1, 2026 upstream and dependency refresh

The `update-all-20261001` branch preserves custom revision
`116500583749fc70c31084d514f8007c61d88d6d` and incorporates upstream main through
`4d3b5413230139aa1791bdb707b50ac4225d80d1`. Upstream fixes release of copied
memory locks, missing remote indexes on push, host binding status, and removal
of old secret-bearing Lance versions during scrub. Native OMP normalization,
lineage/provenance, source protocol 1, and `actomasto-v1` remain unchanged.
The imported remote-cache regression borrows its repository identifier to
match the stable `hf-hub` 1.0.0 builder API used by this fork.

The compatible lockfile refresh advances `async-compression` to 0.4.50,
`compression-codecs` to 0.4.45, `quinn-proto` to 0.11.19, `quinn-udp` to 0.5.16,
and `yoke-derive` to 0.8.4 without changing manifest constraints. Lance remains
pinned to 11.0.0 and Arrow to the 58 series.

Standalone verification uses `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings`,
`cargo clippy --locked --all-targets --no-default-features --features onnx -- -D warnings`,
and `cargo test --locked --all-targets -- --test-threads=1`. Keep
`HF_FUNES_TEST_TOKEN` empty: live-Hub tests publish scratch datasets and are
not authorized by this refresh. The final executable is rebuilt after commit
and its source capabilities must report that exact committed revision before
consumer activation. A synthetic source refresh/enumerate/turns/read round trip
checks strict normalization and provenance without using or changing live
memory. Activation and service restarts belong to the integrating operator;
this source update does not run `funes update` or publish Hugging Face artifacts.

### October 2, 2026 upstream and compatible dependency refresh

The `update-20261002-stable18410` branch starts from custom revision
`eee23d57b6a5895b23898792728c880b3fdd25f8` and merges upstream main through
`767122709efffb98931801f8d664e318abe8d443`. Recall now ranks by relevance
without a recency half-life and accepts exact `YYYY-MM-DD` `--since` / `--until`
bounds through the CLI and MCP. The merge retains native OMP ingestion,
graph provenance, revision-bound source protocol 1, `actomasto-v1`, and the
GitHub-only software release workflow. It keeps the local-source inventory
dependency and subcommand while adopting upstream's updated recall description.

`cargo update` resolved zero package changes: the existing lockfile already
contains the latest versions compatible with its manifest constraints. Lance
remains pinned to 11.0.0 and `object_store` to 0.13.2, with Arrow in the 58 series.
No dependency bounds were widened. Verification below covers only local builds
and synthetic fixtures; `HF_FUNES_TEST_TOKEN` remains empty so credential-gated
Hub tests cannot publish scratch datasets.

Verification used Rust/Cargo 1.98.1, system `protoc`, the default BLAS backend,
two build jobs, dev opt-level 1 with debug info and incremental compilation
disabled, lld, and `RUST_MIN_STACK=16777216`. `cargo fmt --check` and both
warning-free Clippy commands from the October 1 section passed. Library and
binary tests passed 278 and 1 tests respectively. The first all-targets run then
stopped at terminal integration tests because `expect` was absent. Extracting
Arch's Expect 5.45.4 package into a disposable `/tmp` directory supplied that
test prerequisite without a system installation. The resumed
`cargo test --locked --test "*" --examples -- --test-threads=1` passed all 78
reported integration tests and compiled the five example test targets. Total:
357 reported passes, zero remaining failures; six token-gated early returns
are included in that count and their live-Hub behavior remains unverified.
The default executable built with `cargo build --locked --bin funes`.

Actual CLI smoke checks passed capabilities/refresh/enumerate/turns/read with
synthetic OMP originals, preserving full ordinary Unicode text, native
session/message/unit identities, exact raw-record digests, timestamps, and
completion provenance. Metadata responses exposed neither source text nor
private paths; reasoning and provider bookkeeping stayed excluded. Aborted
turns remained pending, repeated reads retained unit identities, unknown request
fields failed closed, changed/missing originals were rejected, and missing
sources remained inventory tombstones. Separate native OMP CLI checks wrote
nothing under `index --check`, resumed bounded indexing from partial to complete
coverage, retained graph parent citations through `get`, and exercised inclusive
same-day recall bounds, exclusion by a future lower bound, and rejection of
noncanonical date spelling. Runtime evidence is retained locally under
`/tmp/funes-update-20261002-{source-smoke,native-smoke}.json`; integration output
is `/tmp/funes-update-20261002-integration-tests.log`.

These checks exercised implementation commit
`ba44cd529d70149c262fbb3a2739361238fffdef`; this follow-on commit changes only this
verification record. Rebuild it and require source capabilities to report the
final full committed SHA before consumer activation. No live installation,
configuration, service, enrollment, memory, hosted generation, release tag,
software release artifact, or Hugging Face upload was changed or published.
Only the dated origin source branch is pushed. Verification assisted by
OpenAI Codex.

### October 2, 2026 OMP 18.4.12 consumer refresh

The `update-20261002-stable18412` branch starts from
`e5783df5eaa5cde5c38e2f0d8c808f0448ff48d9`, not the older root `main` or
the previous `d03a2fc` upgrade. The active Actomasto source exporter reports
that exact steering/mode parser revision; the installed OMP bridge still pins
`d03a2fc`. The new branch preserves both histories and merges upstream through
`33c3c58`. Recall now skips cross-encoder reranking when its candidate pool is
less than four times the requested hits; larger pools retain reranking.
The CLI, MCP descriptions, and recall documentation retain upstream's matching
contract. The merge preserves the fork's `source` subcommand and all native OMP
ingestion, attribution, completion, revision, lineage, and identity gates.

`cargo update` refreshes only `bon`/`bon-macros` 3.10.2 and `libc` 0.2.190
within existing manifest constraints. Lance remains 11.0.0, Arrow remains in
the 58 series, and local source protocol 1 and `actomasto-v1` are unchanged.

Focused verification uses `cargo nextest run --locked` with the local-source
normalizer, OMP parser, index/coverage, and recall unit tests plus
`local_source_normalize`, `omp_index`, `index_check`, `reindex_incremental`,
`recall_without_rerank`, `recall_rerank_line`, and `recall_since_until`.
The runnable default-BLAS executable is built after the final source commit
with the existing optimized dev/lld convention. Consumer probes must require
that executable's capabilities to report the exact final committed revision.
An isolated synthetic CLI smoke covers steering/mode parsing, source metadata
and full-text reads, bounded native indexing and resumed coverage, full
citations, and both fused and reranked recall. These are local checks, not
authorization to publish or upload Hugging Face artifacts or memory.
Installation/configuration cutover and service restarts remain separate
operator-owned steps. AI-assisted integration by OpenAI Codex.
