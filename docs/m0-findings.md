# M0 Findings

Conclusions from M0 prototype validation. Only record what was actually checked.

## Schema source: elasticsearch-specification

Checked 2026-11-12 against `output/schema/schema.json` on `main` (11.5 MB, Apache-2.0).

- Version branches `7.17`, `8.15`, `8.19`, `9.0` each ship their own `output/schema/schema.json`
  (HTTP 200 on raw URL; contents of the older branches were not inspected).
  Per-version packaging can therefore pick a branch instead of filtering by `since`.
- Top level: `endpoints` (617) and `types` (4042). Type kinds: `interface`, `request`, `response`, `enum`, `type_alias`.
- Endpoint: `urls[]` with `path` templates (`/{index}/_search`) and `methods[]`; `request` points at a `request` type.
- Request type: `body.kind == "properties"` holds body properties; `path` and `query` hold URL parameters.
- Property: `name`, `required`, `type`, optional `aliases` (e.g. `aggs`), `deprecation`, `availability`.
  Only 335 of 11624 properties carry `availability`; 275 have `stack.since`. Most properties have no version info,
  so branch-per-version packaging is needed for filtering to be reliable.
- Value types: `instance_of`, `array_of`, `dictionary_of` (user-defined keys), `union_of`, `literal_value`, `user_defined_value`.
- Query DSL variants: `_types.query_dsl.QueryContainer` is an `interface` with `variants.kind == "container"`
  and one property per query type (63 on `main`). `_search` body `query` points at it, so the M0
  "keywords under `query`" completion can come straight from this type.
- Other variant kinds exist on type aliases (e.g. `internal_tag`); not yet surveyed.

Not verified: how `inherits`/`attachedBehaviors` must be merged into a request's properties, and how
`aggregations` containers (`dictionary_of` + variants) should drive completion.

## Prior-art survey (library-first)

Checked 2026-11-12 via crates.io API, `gh api`, and the published `gpui-component` / `gpui-base` 0.9.1 sources.

| Need | Candidate | Finding | Decision |
|---|---|---|---|
| Schema source | `elasticsearch-specification` (Apache-2.0) | Official, versioned per branch, see above | Use |
| Completion data format | Kibana Console `generated/*.json` (613 files, from the spec) | Kibana is AGPLv3 / SSPL / ELv2 (repo `LICENSE.txt`), not compatible with an MIT/Apache project | **Do not vendor or port code.** Design reference only; generate our own index from the spec |
| Rust crate for the ES spec / DSL completion | none found on crates.io | `elasticsearch-dsl` (0.4.26) is a typed query *builder*, not a completion source | Nothing reusable |
| Editor completion UI | `gpui-component` `input` editor | Ships an LSP-shaped `CompletionProvider` trait (`gpui-base` `input/editor/lsp/completions.rs`): `completions(&Rope, byte offset, CompletionContext, ..) -> Task<Result<CompletionResponse>>`, using `lsp-types` 0.97 | Use. Do not build our own popup |
| LSP types | `lsp-types` 0.97 (MIT) | Same crate and version `gpui-base` uses, so completion items can pass through without conversion | Use |
| LSP server framework | `lsp-server` 0.10 (MIT/Apache, rust-analyzer, sync, stdio) | Depends only on `serde_json`, so it pairs with `lsp-types` 0.97 | Preferred for `es-dsl-lsp` |
| | `tower-lsp-server` 0.23 / 0.24-rc | Uses `ls-types` / `gen-lsp-types`, not `lsp-types` 0.97, so types would need conversion | Rejected for now |
| | `async-lsp` 0.2.4 | Pinned to `lsp-types` ^0.95, mismatch with 0.97 | Rejected for now |
| Tolerant JSON parsing | `tree-sitter-json` 0.24 (error-tolerant, incremental) | Tried, see "tree-sitter-json experiment" below: unusable for truncated prefixes | Rejected for cursor-path extraction; keep `cursor_at` |
| | `jsonc-parser` 0.34 | Aimed at valid JSONC; recovery on truncated input not checked | Not evaluated further |

Implications:

- The editor-to-completion seam already exists: `es-deck` only needs a `CompletionProvider` that talks to the LSP, so
  the UI-side completion code should be thin (constraint 3 in CLAUDE.md still holds).
- `es-dsl-lsp` can be `lsp-server` + `lsp-types`; whether the in-app path runs the server in-process or over stdio is undecided.
- `es-dsl-core::cursor_at` stays hand-written: the tree-sitter comparison below found no usable alternative.
- Not verified: `CompletionProvider` behavior with IME and its popup positioning (that is M0 item 1).

## tree-sitter-json experiment

Checked 2026-11-12 with a throwaway crate (outside the repo) using `tree-sitter` 0.27 + `tree-sitter-json` 0.24.8,
comparing against `cursor_at` on 143 inputs (14 hand-written cases plus every prefix of a realistic `_search` body).

- Result: the parse trees of truncated input carry no nesting. Examples (S-expressions):
  - `{"query": {"bo` -> `(ERROR (string (string_content)) (string_content))`
  - `{"query":{"bool":{"must":[{},{` -> `(ERROR (string ..) (string ..) (string ..) (object))`
  - `{"a":"abc` -> `(ERROR (string (string_content)) (string_content))`
  Unclosed `{`, `[` and `:` produce no nodes, so path and key/value slot cannot be read from the tree.
  A `pair` node only appears once the value is complete (`{"a":1,` -> `(document (ERROR (pair ..)))`).
- Recovering the structure would mean re-scanning tokens and matching brackets, which is what `cursor_at` already does.
- Not tested: input with text after the cursor (mid-document edits), where tree-sitter should do better.
  Completion in M0 only needs the prefix, so this does not change the decision. Revisit for diagnostics/hover,
  which run on complete documents.

## Schema-driven key completion (survey and first result)

Checked 2026-11-12 on crates.io and GitHub.

- Search terms: "json schema completion", "json language server", "json schema lsp completion", "jsonc path cursor",
  "elasticsearch query dsl" (crates.io); "elasticsearch query dsl autocomplete language-server" and
  "json schema language server rust completion" (GitHub, no results).
- No crate does schema-driven completion for a JSON document. `elasticsearch-dsl` 0.4 is a typed query builder, not a completion source;
  the LSP-related crates found are unrelated runtimes or language-specific servers.
- Kibana's console definitions stay off limits (licence), see the survey table above.
- Decision: hand-write the descent in `es-dsl-core::complete` (about 100 lines on top of `cursor_at` and the index). It reuses the spec index instead of a keyword table.

Result on the real `main` index (release build, one call each, wall time on this machine, not a benchmark):

- `{"query": {"bo` -> `bool`, `boosting`; `{"query": {"bool": {"must": [{"ma` -> 6 `match*` keys; `match.<field>` -> 13 `MatchQuery` keys. Each call took tens of microseconds.
- `{"aggs": {"x": {` -> `UnknownPath`: spec aliases (`aggs` for `aggregations`) are not in the index yet.
- Only key completion exists; value completion, enums, `inherits` and per-version filtering do not.

Update (same day): `es-deck-schema` now merges `inherits` (child hides parent, cycle-guarded) and expands `aliases` into extra keys.
On the real index (1.0 MB, up from 0.8 MB): `term.<field>` gives `_name`, `boost`, `case_insensitive`, `value` (`boost`/`_name` come from `QueryBase`);
`{"aggs": {"x": {` gives 85 aggregation keys including `aggs`; `te` there gives `terms`.
Still ignored: `behaviors` (`AdditionalProperty`/`AdditionalProperties` field-name keys, as in `terms.<field>`), generics on `inherits`.
Of 617 request types, 615 inherit an interface; only their own body properties are indexed (not checked whether any parent adds body keys).

## es-dsl-lsp (first cut)

`lsp-server` 0.10 + `lsp-types` 0.97, as chosen in the survey; nothing else hand-written for the protocol layer.

- Scope: `initialize`, full-text sync (`didOpen` / `didChange` / `didClose`), `textDocument/completion`, `shutdown` / `exit`.
  Trigger character `"`. Completion items are plain keys (`CompletionItemKind::PROPERTY`), no text edits.
- Empty results are explained with `window/logMessage` (`ValueSlot`, `UnknownPath`, ...), since LSP has no field for it.
- API facts checked in the 0.10 source: `Response` carries `response_result: Result<Value, ResponseError>` (not `result` / `error`);
  `Connection::memory()` allows an in-process end-to-end test.
- `lsp_types::Uri` has interior mutability (clippy `mutable_key_type`), so open documents are keyed by the URI string.
- Verified: unit and in-memory end-to-end tests; the release binary over real stdio with the real 1 MB index
  (`ma` inside `bool.must[{` gives 6 `match*` keys; exit code 0 after `shutdown` + `exit`).
- Not verified: any real editor client, the gpui `CompletionProvider` path, IME (M0 item 1), UTF-16 columns against a real client.
- The `es-dsl-lsp` binary needs the index path as its first argument; no bundling or version selection yet.

Request-line completion (added while the GPUI build ran): on a request line `complete` now offers methods (`PO` -> `POST`) and the next literal path segment
from the endpoint URL templates (`GET /_cat/he` -> `health`; `POST /my-index/_sea` -> `_search`, `_search_shards`, `_searchable_snapshots`).
Placeholder segments such as `{index}` are skipped (no mapping/index names yet) and query-string parameters are not indexed.
This replaces the misleading `ValueSlot` previously returned on a request line.

## GPUI CompletionProvider wiring (editor -> LSP -> popup)

- `es-deck` runs `es-dsl-lsp::serve` in-process over `Connection::memory()` (`lsp_client.rs`); `LspCompletion` (`completion.rs`) implements GPUI Kit's `CompletionProvider` and calls it from `cx.background_spawn`, so the UI thread never blocks.
- Headless test `console::tests::typing_a_key_prefix_completes_through_the_lsp` (`gpui_kit::test`, `test-support` feature): typing `q` after `{\n  "` opens the `completion-menu`; Enter yields `query` with no duplicated quote, even though items carry no `textEdit`.
- Not verified: IME / preedit, popup positioning on a real display, Windows and macOS builds.

## Dependency pitfalls hit

- `gpui-kit` pulls `gpui-fast` with `taffy =0.15.0`; an old lockfile pinned 0.15.1 and `cargo update --precise` failed. Regenerating `Cargo.lock` fixed it.
- Do not add `ropey` 1.x: gpui-base uses ropey 2.0.0-beta.1. Use `gpui_kit::component::Rope`.
- A stale cargo index cache hid `proptest 1.12`; deleting its cache entry fixed it.
- Linking the test binary needs `libxkbcommon-x11`, `libxcb-xkb` and `libx11-xcb` dev packages. Without sudo: `apt download` the `-dev` and runtime debs, `dpkg -x` them to a private dir, and set `LIBRARY_PATH` (build) and `LD_LIBRARY_PATH` (run) to its `usr/lib/x86_64-linux-gnu`. This avoids a full rebuild, unlike `RUSTFLAGS`.

## Version filtering (`availability.stack.since`)

- The spec only carries `since` (plus stability and visibility); it records no removals, so a key that was dropped in a later version still shows on newer clusters. 462 of 617 endpoints and 251 properties in the index have a `since`.
- `Prop` and `Endpoint` gained `since: Option<[u32; 3]>`. `complete(.., version)` hides anything newer than the cluster, and an unknown version hides nothing. `es-dsl-lsp <index.json> [version]` and `es-deck <index.json> [version]` take the version as an argument until connections exist.
- Real index, `POST i/_search` body keys: 36 unfiltered, 33 on 7.0 (`knn`, `rank`, `retriever` hidden), 35 on 8.11.
- Not done: `serverless` availability, `stability` (experimental/beta) and `visibility: private` are ignored; `featureFlag` is ignored.

## Query-string parameter completion

- `Endpoint.query` holds the request's `query` properties plus those of its `attachedBehaviors` (`_spec_utils.CommonQueryParameters`: `pretty`, `human`, `error_trace`, `filter_path`); 615 of 617 requests attach it. Only names are indexed; value completion is not done.
- `POST idx/_search?pre|` offers `preference`, `pretty`; names already in the query string are skipped, `name=|` reports `ValueSlot`, and version filtering applies.
- Not done: path placeholders (`{index}` names) and `AdditionalProperty` behaviors.

## es-client HTTP stack survey

Needs: arbitrary method, path and raw body (console semantics); Basic and API Key auth; TLS incl. custom CA; timeouts; cancelling an in-flight request. `es-client` must not depend on UI crates.

| Candidate | License | Status (crates.io) | Verdict |
|---|---|---|---|
| `elasticsearch` (official) | Apache-2.0 | No stable release; latest is `8.19.0-alpha.1` / `9.1.0-alpha.1` (2025-08). Typed API on top of `reqwest` + `tokio` | Rejected: alpha only, and the console sends raw requests, so the typed API adds nothing |
| GPUI `http_client` | Apache-2.0 | Only a trait in this snapshot, with no TLS client; it is a UI-stack crate | Rejected: violates the dependency direction |
| `ureq` 3.5 | MIT/Apache-2.0 | Active (2026-11), sync, no tokio | Rejected: an in-flight request cannot be cancelled, only abandoned |
| `isahc` 2.0 | MIT | Small user base, libcurl based | Rejected: system library dependency |
| `reqwest` 0.13.5 | MIT/Apache-2.0 | Active (2026-09), ~263M recent downloads, tokio based | **Chosen**: dropping the future (or `JoinHandle::abort`) cancels the request |

Choice: `reqwest` with `default-features = false, features = ["rustls"]`. Probe in a scratch crate: resolves to 123 crates, `cargo build` took 27 s on this Linux box. It uses `aws-lc-rs` as the crypto provider and `rustls-platform-verifier` (the OS trust store). `tokio` and `hyper` are already in `Cargo.lock` through GPUI.

Integration plan: `es-client` owns a small tokio runtime and exposes async calls; `es-deck` awaits the `JoinHandle` inside `cx.background_spawn`, and aborting the handle cancels the request.

Test doubles: `httpmock` 0.9 (MIT, active 2026-10) as a dev-dependency; real-cluster tests stay behind `it-cluster`.

Later, credentials: `keyring` 4.2 (MIT/Apache-2.0). Not probed: it needs a Secret Service (D-Bus) on Linux.

Unverified: `aws-lc-sys` on Windows and macOS (it may need a C toolchain or NASM there); `ring` through `rustls-no-provider` is the fallback.

## Reference: decompiled commercial JetBrains plugin (design reference only)

Checked 2026-11-16 by reading `C:\Code\elasticsearch4idea-vineflower-sources` (decompiled, obfuscated, commercial). Licence: do not copy code or data tables; used only to see what a finished product covers.

- It hand-writes its API tables (`EsRestApiCatalog`, `SearchApiSpecs`, `QueryDslSpecs`, ... 1000+ lines each) and special-cases bodies by request kind (search, reindex, create-index mapping, bulk). We keep generating from the spec instead.
- Capabilities worth matching, mapped to PRD items: index-name completion from the cluster (A5), field completion from live mapping with a same-nested-path filter (A4), snippet-style insertion of whole clauses (A3), documentation popups (A8), JSON duplicate-key and syntax inspections (A9), bulk bodies parsed as alternating action/source lines (E4).

## Value completion (first slice of "complete" coverage)

- `Index.enums` (spec `enum` members) plus `TypeRef::Bool` / `TypeRef::Literal` feed value completion. Value slot descends the same path as key completion; outside a string, string values come back quoted, inside an open string they are unquoted and prefix-filtered (`Slot::Str`).
- Table tests cover enum, bool, literal and unknown-path cases against a synthetic index. Not yet run against the real 11 MB spec, so enum coverage there is unchecked.
- Still missing for value completion: `type_alias` expansion (e.g. aliases to enums or unions), numeric ranges, unquoted-token prefixes (`tr|`), snippets for object-valued properties.

Update (same day): `es-deck-schema` expands non-generic `type_alias` inline (depth limit 8, so self-referencing aliases terminate); generic aliases stay plain names.
On the real `main` spec: index grows from 1.0 MB to 1.7 MB, 406 enums, 2186 of 12705 properties offer values, including `_search` body `explain`, `track_total_hits`, `profile`, `_source`, `track_scores`, `version`, `seq_no_primary_term`, `search_after`.
Not checked: whether every one of those values is sensible (e.g. `search_after` offering a literal), and `sort` (its value type is a union of object forms).

## Open keys (`AdditionalProperty` / `AdditionalProperties` behaviors)

- `Index.open` maps a type to `OpenKey { is_field, value }`; `es-deck-schema` reads the behavior's generics (`K`, `V`) and inherits it from ancestors. `is_field` is true when `K` is `_types.Field`, so mapping field names can supply key candidates later.
- Descent: a key not declared on a type falls through to its open key's value type (declared keys win). So value completion works under e.g. `terms.<field>`.
- Real `main` spec: 65 types have an open key, 13 of them field-keyed (`TermsQuery`, `GeoBoundingBoxQuery`, `GeoDistanceQuery`, `GeoShapeQuery`, decay functions, ...).
- Not done: key candidates for open keys (need mapping, step 4), generic `V` parameters, `OverloadOf` behaviors (21 types, not surveyed).
- Not verified end to end on the real index through the LSP; only table tests on a synthetic index and the converter test.

## NDJSON and value-typed bodies

- Spec request bodies come in three kinds: `no_body` (349), `properties` (234), `value` (36). Value bodies were ignored before. Now `Endpoint.body_ty` holds a single-type body (29 endpoints on `main`, e.g. `PUT _settings` -> `IndexSettings`) and `Endpoint.lines` holds NDJSON line types (7 endpoints: `bulk`, `monitoring.bulk`, `msearch`, `msearch_template`, `fleet.msearch`, `ml.post_data`, `text_structure.find_structure`), taken from `requestMediaType: application/x-ndjson` and the element union.
- Line roles are not in the spec, so the engine uses one small rule: with 2 line types, lines alternate header/body; with 3 (bulk), a line following an action is its payload, except after `delete` (no payload line) and `index`/`create` (the payload is a user document, reported as `NoCompletion::FreeForm`); `update`'s payload is the update action. One type means every line has that type.
- The cursor line is scanned on its own, so the JSON path restarts at each line. Blank lines are skipped when counting.
- Not done: multi-line (pretty-printed) JSON inside an NDJSON body; the 29 value bodies were only checked through the converter and a synthetic table, not by hand against the real index; `OverloadOf` behaviors.

## Field-name completion from mapping (first version)

- Core: `Fields` trait (`fields(target_index) -> Vec<Field>`), `NoFields`, and `parse_mapping` for a `GET /{index}/_mapping` response (objects, nested, multi-fields, merged across indices). The engine does no I/O. `complete_with(.., &dyn Fields)` is the new entry point; `complete` keeps its signature. `es-dsl-lsp` has `serve_with(.., &dyn Fields)`; `serve` passes `NoFields`.
- Schema: `TypeRef::Field` (spec `_types.Field`, checked before alias expansion) and `TypeRef::FieldDict` (a `dictionary_of` keyed by `_types.Field`).
- Field names are offered (a) as values where a property takes a `Field`, through arrays and unions, quoted outside a string; (b) as keys under a `FieldDict` or an open key with `is_field` (`match.<field>`, `terms.<field>`), together with declared keys.
- The target index is the path segment standing for `{index}` in the matching URL template; `""` when the path has none. With no mapping for it the result is `NoCompletion::NoMapping`, not an empty list.
- Not done: filtering by field type (PRD A4 asks for numeric/date only under `range`). The spec does not say which field types a clause accepts, so this needs either a rule table or per-clause metadata; left out of this version. Also not done: fetching mappings (`es-client` call, cache, refresh), wiring a `Fields` impl into `es-deck`, large-mapping performance (PRD 6.3), index/alias name completion (A5).
- Not verified: behavior on a real cluster mapping; only synthetic mappings in tests.

## Candidates with kind and detail

- `candidates_with` returns `Candidate { label, kind, detail }` (`Kind`: Method, Path, Param, Key, Value, Field). `complete` / `complete_with` stay as label-only wrappers so existing table tests are unchanged.
- `detail` is a one-line schema type for keys and parameters (`Query`, `Query[]`, `{field: Query}`, `boolean`, unions joined with ` | `) and the mapping type for fields (`keyword`, `text`, ...). Types the index cannot describe (`Other`) give no detail.
- `es-dsl-lsp` maps `Kind` to `CompletionItemKind` and fills `CompletionItem.detail`. Items still carry no `textEdit` or snippet: object-valued keys do not insert `{}` yet, because the editor's quote handling was only verified for plain labels (see "GPUI CompletionProvider wiring").
- Not done: snippets for object/array values, documentation text (the index holds no descriptions; hover and docs need them added at build time and will grow the index), sort priority beyond alphabetical, and the `es-deck` popup rendering of `detail` and icons (the headless UI test was not run here, see the missing `libxkbcommon-x11`).

## Completion benchmark cases and proptest

- `tests/completion_cases/*.cases` (32 cases, written from the search / query DSL / bulk / msearch / reindex / mapping reference examples): request text with a `|` cursor, `has:` labels that must be offered, `lacks:` labels that must not, `none:` for no candidates. Runner: `tests/completion_cases.rs`, wired as a test of `es-deck-schema`.
- The real `schema.json` is not committed, so the runner needs `ES_SPEC_SCHEMA=<path>`. Without it the test prints a notice and does nothing, i.e. a plain `cargo test` does NOT exercise these cases.
- Result on the 8.x spec, version 8.15.0: 32/32 pass after one case was adjusted. That case was an empty editor expecting method names. The engine deliberately offers none for an empty request-line prefix, so the case now types `P|`.
- The runner also completes at every prefix of every case on the real index (no panic).
- `es-dsl-core` has a proptest (`never_panics_on_random_text`) over random request/JSON fragments with the cursor anywhere, on the synthetic index.
- Not measured: Top-5 hit rate (G1) over a representative sample. 32 hand-picked cases are a smoke set, not a rate. Mapping fields in the runner are a fixed stub, and the cases have not been cross-checked against Kibana Console output.

## Index and alias name completion (engine side only)

- `Fields::names()` (default empty) supplies index/alias names; the request-line completer offers them (`Kind::Index`, LSP `MODULE`) wherever a matching URL template has `{index}` at the cursor segment. Comma lists complete the last element.
- `parse_names(cat_indices_json, alias_json)` turns `GET _cat/indices?format=json` and `GET _alias` responses into one sorted, deduplicated list.
- Not done: nothing fetches those two calls yet (`es-client` is not wired to `es-deck`), so in the app the list is empty. The same applies to mappings. Unverified: whether the editor replaces only the last comma element or the whole word when accepting.

## Cluster metadata cache (es-client, not yet wired into the app)

- `es_client::ClusterMeta` implements `Fields`: `fields(target)` / `names()` only read a shared cache (never block); a missed target is remembered, and the async `refresh(&Client)` reloads `_cat/indices` + `_alias` and fetches `GET {target}/_mapping` for every remembered target. Only targets made of `[A-Za-z0-9-_.*,]` are fetched, so a typed path cannot add segments or query strings to the request.
- Tested against a mock server (miss → refresh → hit; unsafe target never requested). Not tested: a real cluster, large mappings, refresh scheduling and invalidation (a mapping is cached until the process ends), 401/403 handling beyond "keep the old data".
- Not wired: `es-deck` has no connection config yet, and credentials may only come from the OS keychain, so nothing constructs a `Client`, calls `refresh`, or passes the cache to `serve_with`.

## Connection settings (minimal) and metadata refresh

- Research: `keyring` 4.2 (MIT OR Apache-2.0, default `v1` feature = Keychain / Credential Manager / Secret Service, `Entry::{new,set_password,get_password,delete_credential}`) for secrets; `dirs` 6 for the per-user config directory. Both are new dependencies; `keyring` pulls in the platform keychain crates (zbus on Linux).
- `es_client::connection`: `Connection { name, url, auth, user }` saved as `<config dir>/elasticsearch-deck/connections.json` with no secret; secrets (password / API key / bearer token) go to the keychain under service `elasticsearch-deck`, account = connection name. `Secrets` trait with the `Keychain` implementation. `Auth::Bearer` added (sent as `Authorization: Bearer`, redacted in `Debug`).
- `es-deck`: `ConnectPanel` above the editor: name, URL, auth kind (none / Basic / API key / Bearer), user, masked secret, Connect. An empty secret field keeps the keychain value. On success it emits `Connected`; `Console` restarts the in-process LSP with the cluster version and the `ClusterMeta`, and refreshes metadata every 2 s (names at most every 30 s; mappings for targets the completer asked for).
- Verified: compiles, clippy `-D warnings`, `es-client` unit tests (file round trip without secrets, secret requirement per auth kind, Bearer header on the wire via mock server, redaction).
- NOT verified: the panel has never been rendered or clicked (the `es-deck` binary and its headless tests do not link here: missing `libxkbcommon-x11`); the keychain was never exercised (no Secret Service in this environment); no real cluster. The old CLI index/version arguments still work as the pre-connect default.
- Not done: a list/switcher and deletion of saved connections (only the first saved connection is loaded, and Connect upserts by name); TLS options (C3), read-only mode, danger confirmation; renaming a connection leaves its old keychain entry behind; metadata is cached per process with no invalidation after a mapping change; a refresh failure is silent.

## Hover and diagnostics (PRD A8, A9)

Research: no crate does this for the ES Query DSL; Kibana Console has no schema-based diagnostics. Hand-written on top of the completion walk (same endpoint match, body root, `resolve`), with `serde_json` for syntax errors and the spec's own `description`, `serverDefault`, `deprecation` for hover. Nothing is a keyword table.

- Index: `Prop` gained `doc` (first paragraph, at most 240 chars), `default` (spec `serverDefault`) and `deprecated`. The index grows from 1.8 MB to 3.3 MB, because inherited properties repeat their text in every child type.
- Hover: key under the cursor shows type, description, default, since-version and deprecation; the request line shows the endpoint name, URLs and since-version.
- Diagnostics (all Warning except syntax): unknown body key, key newer than the cluster version, deprecated key, unknown query-string parameter, unknown or too-new endpoint, JSON syntax error (per document; per line for NDJSON).
- A key is flagged only when every type on its path is a closed interface in the index. Enums, user-defined values and unresolved paths are never flagged. `tests/diagnostic_cases/valid.cases` (17 real-world-shaped requests) must stay clean; `invalid.cases` (6) must warn. Both ran against the 8.x spec and pass.
- LSP: `hoverProvider`, and `publishDiagnostics` on open/change (cleared on close). The app re-checks 400 ms after the last edit.

Not done: type mismatch and missing-required diagnostics. The spec's `required` and the shorthand forms (e.g. `match: {field: "text"}`) make a naive check noisy; they need the shorthand table first. Values are not checked at all. Hover on query-string parameters and on field names is missing. Hover and diagnostics were not exercised in the real editor: the GPUI headless tests cannot link on this machine.

## Console layout and connection dialog (PRD C1)

Redone after review against `docs/gpui-kit.md`: the first cut put the whole connection form above the editor, which buried the main task. Now the console is a toolbar plus the editor. The toolbar holds one ghost button (the connected name, or `Connect…`), and the Elasticsearch version in muted text. The button opens a dialog: saved connections on the left (the selected one has the selected surface), a labelled form on the right (Name, URL, Authentication, then User or Password/API key/Token only when the auth kind needs it). `Connect` is the only primary button and closes the dialog on success; a failure stays next to the actions. `Delete…` opens an alert dialog (`Delete connection "x"?`, button `Delete connection`) because the saved secret cannot be recovered.

An environment tag (dev/test/prod) was considered and dropped on the user's decision: this is a general-purpose ES tool and the tag only ever drove a banner. It is no longer part of PRD C1, and the former S1 requirement was removed. Danger-request confirmation (S2) applies to every connection.

Not verified: none of this was rendered or clicked (`es-deck` does not link here), so spacing, dialog size and focus order are unchecked; the keychain delete was never run against a real store. Not done: no Send button or response view yet (the console cannot run a request), editing a name leaves the old keychain entry, no TLS options (C3), no readonly mode, no danger-request confirmation (S2, S3), UI strings are English only (PRD U5).

## Sending, hits table, danger confirmation and readonly (PRD A, S2, S3)

- Ctrl/Cmd+Enter or Send runs the request block under the cursor (`es_client::Request::at`); a later key binding overrides the editor's own Ctrl/Cmd+Enter. `pretty` is appended so the server indents the JSON.
- Decision: running only the request under the cursor is enough, matching Kibana Console; "run selection" and "run all" are dropped, and PRD E2 was narrowed to match. A document with several requests is still run one request at a time by moving the cursor.
- Search responses render as a `DataTable` (`es_client::parse_hits`): `_id`, `_index`, `_score` plus the union of top-level `_source` keys. No paging, sorting or filtering yet; the user edits `from`/`size`.
- `es_dsl_core::danger` asks before `DELETE`, `_delete_by_query`, `_update_by_query`, `_close`, `_forcemerge` and a `_bulk` that contains `delete`. It applies to every connection.
- `es_dsl_core::is_write` drives the readonly switch. The spec has no read/write flag, so a short list of POST/PUT reads (`READS`) is kept by hand; anything not listed counts as a write.
- Not verified: rendering, key-binding precedence, a real cluster. The readonly switch is per session and not persisted.
- TLS (PRD C3): a connection can name an extra CA file (PEM, read at connect time in the background) and can skip certificate verification. Both are saved in `connections.json` (no secrets); the skip checkbox shows a warning while on. Not verified against a real TLS cluster; client certificates (mTLS) are not supported.
- Sidebar: a filterable list of index and alias names (`targets.rs`, GPUI Kit `List`), shown once connected and refreshed with the metadata loop. Enter or a click appends a starter `GET <name>/_search` to the editor. Indices and aliases are one flat list (`ClusterMeta::names` merges them); not verified by rendering, and the filter test cannot run on this machine (es-deck does not link here).
- History (PRD H1, H3): `history.json` next to `connections.json`, newest 200 requests, shown per connection in a searchable dialog (`History…`); picking one appends it to the editor. `Clear history` empties the current connection's entries. Requests to `_security` and `_xpack` are not recorded because their bodies carry credentials. Favorites with groups and tags (H2) are not built. Request bodies are stored as typed, so other sensitive data in a body is kept.
- Format (PRD E3, partly): `es_dsl_core::format_body` re-indents the body of the request under the cursor (Format button, Ctrl/Cmd+Shift+F). It skips bodies with comments, NDJSON and invalid JSON. Compact/minify, formatting selections and bracket matching are not built. The formatted text replaces the whole document through `replace_all` so undo works, but that resets the scroll position.
- Timeout (PRD V7): per connection, whole seconds, saved in `connections.json`; empty or invalid keeps 30 s. The in-flight Cancel button already covers cancellation.
- Theme (PRD U2): View offers Follow system appearance, Light and Dark, with a check mark on the current choice. The choice is saved as `appearance.json` next to `connections.json` (one of `"system"`, `"light"`, `"dark"`; an unreadable file falls back to system). In system mode an OS appearance change still applies; in Light or Dark it is ignored until the user picks System again. The kit resets the radii on every theme load, so they are re-applied after the mode. Not verified by rendering, and a real OS appearance change was not exercised; on Linux the kit notes that `window.appearance()` is the reliable source, which is what system mode uses.
- Response tree and save (PRD V1, V4): any JSON response also gets a collapsible Tree view (`es_client::json_tree`, nodes addressed by JSONPath-style paths such as `$.hits.hits[0]._id`, 20,000-node cap) with `Copy path`; a non-search JSON response opens in JSON, a search in Table. `Save…` writes the whole body, not the 2 MB the JSON view shows, through the platform's save dialog. There is no success message after saving and no tree search. Not verified by rendering; the file dialog may be unavailable on some Linux setups, in which case Save does nothing.
- Highlighting: the editor draws no syntax colors unless gpui-kit is built with `tree-sitter` (JSON grammar, MIT) or `syntect`; the workspace now enables `tree-sitter`. Icons likewise need an `AssetSource` (`crates/es-deck/src/assets.rs`). The kit's `Assets` embeds only the 106 icons in its `default-icons.txt`, so any other `IconName` has to be listed in `ExtraIcons`; `Lock` and `LockOpen` were missing and drew nothing until they were added. Both were verified only by compiling; not yet seen rendered.
- Console tabs: kept as the hand-drawn strip, not `gpui_component::TabBar`. `Tab::icon` draws the icon *instead of* the label (`tab.rs` only renders the label when `icon` is `None`), so icon+label tabs silently lose their names; `Tab` also has no context-menu hook, which would drop Rename/Delete. The strip stays a flex row with an active tint, a close button and the Rename/Delete menu; `Ctrl+Tab` / `Ctrl+Shift+Tab` cycle and `Cmd/Ctrl+W` closes. The response view switcher still uses `TabBar`, with the icon in the `prefix` slot so its labels show. Tabs past the panel width are clipped; the shortcuts are the way to reach them.

## Completion popup: icons, footer hint (survey, no code yet)

Goal: match the reference plugin's popup: a kind icon per row, the type right-aligned, and a footer line such as "Press Enter or Tab to replace". Checked in gpui-component / gpui-base 0.9.1.

What the kit gives:
- The popup is `CompletionMenu` in `gpui-component/src/input/popovers/completion_menu.rs`. A row is label + `detail` (muted italic, right). No icon slot and no footer; `CompletionItem.kind` is ignored when drawing. Only two things are configurable from outside: the label highlighter (`Lsp::set_completion_highlighter`, already used for subsequence marks) and the width (`Lsp::completion_menu.max_width`).
- Everything needed to draw the popup (`CompletionMenu`, `CaretAnchoredPopup`, `editor_popover`) is `pub(crate)`. The editor overlay builds it unconditionally (`build_lsp`), and it opens whenever `EditorState::completion_menu_state().open` is true, so a second popup in the app would draw on top of it.
- The state is public: `completion_menu_state()` (open, query, items, revision; doc-hidden), `insert_completion`, `dismiss_completion_overlay`, `route_overlay_action`, `cursor_bounds()` and `prepaint_caret_geometry()` for the anchor. gpui-base documents the revision as "a renderer that mirrors this menu" uses it, so mirroring is an intended use, but gpui-component offers no way to swap its renderer.
- No third-party crate does this: no editor-popup crate exists for GPUI, and the ones that draw lists are not caret-anchored.

Options:
1. Patch the kit's `completion_menu.rs` (copy `gpui-component` into the repo, point `[patch.crates-io]` at it, change the row to add an icon by `CompletionItemKind` and add a footer to `editor_popover`'s content). About 50 lines changed; trigger, debounce, IME, blur and scroll behaviour stay the kit's. Cost: a vendored fork of Apache-2.0 code that has to be re-applied on every kit upgrade (CLAUDE.md already makes upgrades their own commit).
2. Own pipeline in `es-deck`: the provider always returns nothing (so the kit menu never opens), a `completion_popup` module requests from the LSP on `InputEvent::Change`, drops stale answers, draws a `deferred` popup at `cursor_bounds()` and handles Up/Down/Enter/Tab/Esc through its own key context. No fork, but it re-implements trigger, dismissal on blur or cursor move, scrolling and the IME interplay, and the IME check is the open M0 risk.
3. Wait for the kit to add an icon slot and footer. Cheapest; no date.

Recommendation: option 1. It is the smallest change that gets the result, and it keeps the risky behaviour (IME, focus) in code the kit already tests. Nothing else in the app needs the fork. Fall back to option 2 only if the vendored copy proves hard to carry.

Open: the type column depends on the LSP filling `detail` (`es-dsl-lsp` passes `Candidate`'s detail through; not checked that every kind has one). Not verified: none of the above was run.

Decision: the user rejected the vendored fork (option 1) and chose to stay on the stock kit, so the popup keeps no icons and no footer; only the label highlighter is customised. Revisit if the kit adds an icon slot (option 3).

## Per-version index download

- On connect, `es-deck` picks the spec branch for the cluster's `major.minor`, then up to six lower minors of the same major (the spec has no 7.18 or 7.19 branch; 7.17 is the last 7.x). The converted index is cached as `index-<branch>.json` in the config directory.
- Checked against raw.githubusercontent.com: a 7.19.0 cluster tried 7.19 and 7.18 (HTTP 404), downloaded 7.17 and cached it, about 10 s in total.
- `crates/es-deck/assets/index-8.19.json` is bundled and used until the download finishes or when it fails. Regenerate it with `es-deck-schema` from the 8.19 branch's `schema.json`.
- The download URL template can be replaced with `schema-url.txt` in the config directory (`{branch}` is substituted). Not checked against a real mirror.
