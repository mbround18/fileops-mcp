# Contributing

## Layout

```
apps/fileops-mcp/     binary crate: the MCP (rmcp) adapter, stdio transport
crates/fileops-fs/    library crate: all logic and all rendering
docs/guides/          user-facing documentation
```

`apps/*` is an execution layer only: parameter schemas, translation into core requests,
and the tool result. Everything worth testing lives in `crates/fileops-fs`, including the
text each tool renders — the output format is the product, so it is tested where it is
produced, not through the transport.

## Architecture

Ports and adapters, with the filesystem as the only dependency:

```
         ┌───────────────────────────────────────┐
  MCP ──▶│ fileops-fs (domain)                   │──▶ std::fs · ignore::Walk
 stdio   │  read · grep · find · inspect         │     (real paths, read-only)
         │  slice · walk · text · budget         │
         └───────────────────────────────────────┘
```

* `budget.rs` — the shared byte ceiling. `Budget::new(Option<usize>)` clamps a request to
  `MAX_MAX_BYTES` and falls back to `DEFAULT_MAX_BYTES`; `try_spend` refuses rather than
  overshooting, which is why no response can exceed its own stated limit.
* `text.rs` — reading a path into `Content::{Text, Unavailable}`, the `Status` vocabulary
  (`missing`, `directory`, `binary`, `denied`, `error`) and its one-line `note()`, binary
  sniffing (a NUL in the first 8 KiB), `lines()` (matches `wc -l`, strips CR), and the
  compact size/count formatters (`792B`, `4.2K`, `6.1k`).
* `slice.rs` — 1-indexed inclusive `Span`s and everything done to them: `parse` (`12`,
  `12-40`, `40-`, comma lists), `normalize` (clamp, sort, merge adjacent), `window`
  (`lines` wins; `head` and `tail` combine), `with_context`, `intersect`, `cap`,
  `describe` (`12-40,98-120`). Pure functions, no I/O.
* `walk.rs` — path resolution (`~`, relative to `cwd`), glob detection and expansion,
  `GlobSet` construction (`literal_separator(true)`, so `src/*.rs` does not reach into
  `src/deep/`), and the `ignore::WalkBuilder` setup: `require_git(false)` so `.gitignore`
  is honoured outside a repository, `sort_by_file_name` for determinism, and `.git` never
  entered.
* `read.rs`, `grep.rs`, `find.rs`, `inspect.rs`, `outline.rs`, `extract.rs`, `survey.rs` —
  one module per tool. Each takes a
  request holding a *list*, spends from one `Budget`, and returns both the rendered text
  and the structured outcome behind it. `extract.rs` parses JSON, YAML and TOML into one
  `serde_json::Value` so a single dotted-path selector works across all three;
  `survey.rs` aggregates a walk rather than listing it, which is why its output is
  bounded by the number of file types rather than the number of files.

### Invariants

These are the reason the crate exists. Do not relax them without a very good argument:

1. **Every tool takes a batch.** `read` takes `specs`, `grep` takes `patterns` and
   `paths`, `find` takes `roots`, `inspect` takes `paths`. A tool that accepts a single
   path pushes the cost back into round trips, which is the thing being fixed. New tools
   take lists too.
2. **The budget cannot be removed.** There is no "unlimited" value. `max_bytes` is an
   `Option<usize>` that clamps into `[0, MAX_MAX_BYTES]`, and
   `a_request_cannot_opt_out_of_the_budget` asserts it. Every renderer spends through the
   same `Budget` so one response has one ceiling.
3. **Truncation is always reported.** A cap that bites appends a marker — `+82 lines cut`,
   `(3 of 100)`, `(skipped: byte budget reached)` — sets `truncated` in the structured
   outcome, and is named in the footer. Output must never simply stop.
4. **A per-path failure never fails the call.** A missing, binary, unreadable or
   directory path renders as one status line and the rest of the batch is delivered. Tool
   errors are reserved for a request that cannot be interpreted at all (an empty batch, an
   unparseable range, a bad regex or glob) — and then the message says what to write
   instead.
5. **Nothing is unbounded by default.** `DEFAULT_MAX_PER_FILE = 20`,
   `DEFAULT_MAX_MATCHES = 200`, `DEFAULT_LIMIT = 500`. Defaults exist so that a careless
   call is cheap; the caller raises them deliberately.
6. **Filters apply to what was found, never to what was named.** `.gitignore`, hidden
   files, `glob`, `exclude` and `depth` narrow a walk. A path given outright is read
   regardless — that is why `grep` carries `Vec<(PathBuf, bool /* walked */)>`.
7. **`.git` is never walked.** The walker's `filter_entry` drops it unconditionally, at
   any depth, with or without `hidden`/`no_ignore`.
8. **Line numbers are on by default.** `number: true` in both the serde default *and*
   `impl Default`. A slice you cannot locate is a slice you cannot edit against. When the
   two defaults disagree the bug is invisible in unit tests and visible over the wire, so
   any field with a serde default gets a hand-written `Default` that matches.
9. **Everything is read-only.** No module in this workspace writes, moves or deletes a
   path. There is no `write`, no `edit`, no `mkdir`. A contribution that mutates the
   filesystem belongs in a different server.
10. **stdout belongs to the protocol.** Logging goes to stderr only, via `tracing`. A
    stray `println!` corrupts the JSON-RPC stream.
11. **Determinism.** Walks are sorted, dates are computed arithmetically
    (`civil_from_days`) rather than through a calendar crate, and tests read and write
    only inside their own tempdir.
12. **Nothing is sent twice.** The rendered text travels in the response's text block and
    nowhere else: every outcome's `text` field is `#[serde(skip_serializing)]`, so the
    structured copy carries the counts and never a second copy of the bytes the caller is
    already reading. `no_response_pays_for_the_same_text_twice` asserts it for every tool.
    A 40-line `read` costs 2.1k on the wire; it cost 4.1k when the text was serialized
    too.

### Language patterns

`outline.rs` holds one regex per language family and no parser. They are tuned to miss
rather than to guess: a line that obviously declares something is kept, anything clever is
left to `grep` or to the caller's own `pattern`. A new family is a match arm, a `name`, and
a test asserting the exact outline of a small real-looking file — not a new dependency.

## Testing

```bash
make check     # fmt-check + clippy (warnings denied) + the full suite
make test
```

* **Unit tests** live beside the code in `crates/fileops-fs`, build a real tree in a
  tempdir, and assert the *exact* rendered text. A rendering change is not finished until
  a test pins the text it produces; `the_footer_counts_what_was_actually_emitted` is the
  shape those tests take.
* **End-to-end tests** in `apps/fileops-mcp/tests/` spawn the built binary
  (`CARGO_BIN_EXE_fileops-mcp`) and speak JSON-RPC over its stdio: the tool list, the
  schemas, the server instructions, one batched `read` asserted character for character,
  and the readable error a malformed request comes back as.
* Test names are sentences describing the behaviour, not the function
  (`one_read_call_replaces_a_chain_of_shell_reads`).

## Adding a tool

1. A module in `crates/fileops-fs`: a `…Request` holding a list plus `cwd` and
   `max_bytes`, a `…Outcome` carrying `text` *and* the structured rows behind it, and a
   free function from one to the other. Re-export all three from `lib.rs`.
2. Unit tests for the behaviour, the caps, the per-path failures and the footer.
3. A `…Params` struct in `apps/fileops-mcp/src/server.rs` with `JsonSchema` + `Deserialize`
   and a doc comment on every field — those comments are the agent-facing documentation,
   so write them for a reader deciding whether to call the tool.
4. A `#[tool]` method that maps params to the request, resolves the budget through
   `self.budget(requested)`, and attaches the outcome with `with_structured`.
5. `INSTRUCTIONS` and the `README.md` table, then `docs/guides/output-format.md` for the
   rendering and `docs/guides/token-efficient-reading.md` if it changes how to batch.
6. An end-to-end test, and the tool name in `.claude/settings.json`.

## Conventions

* Rust 2024, latest stable. `cargo clippy --all-targets -- -D warnings` must pass.
* Errors are a `thiserror` enum whose messages tell the caller what to write instead
  (`BadRange` names the forms it accepts). An error the model cannot act on is a bug.
* Comments explain a decision that is not obvious from the code. The module header says
  what the module is for; it does not restate its functions.
* Commits are small, atomic and **signed** — via the `gitops` MCP server where available,
  otherwise `git commit -S`. Never `--no-verify`, never an unsigned fallback.

## Installing a working copy

The registered MCP server runs the *installed* binary, so reinstall after changing code:

```bash
make install
```
