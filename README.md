# fileops

An MCP server that reads the filesystem without spending a context window on it.

## Why this exists

Ask an agent to orient itself in a repository and you get a command like this:

```bash
cd /home/me/project; echo '=== EXT ==='; cat .specify/extensions.yml | head -60; \
echo '=== INIT ==='; cat .specify/init-options.json; echo '=== FEATURE ==='; \
cat .specify/feature.json; echo '=== TEMPLATES ==='; ls .specify/templates .specify/presets; \
echo '=== SPEC ==='; head -90 specs/010-file-slice-dedupe/spec.md; \
echo '=== TASKS ==='; grep -n '^- \[ \]' specs/010-file-slice-dedupe/tasks.md | head -30
```

It works, and it is the right instinct — one round trip instead of eight. But it is paid
for twice. The command itself is long enough to be its own paragraph, and what comes back
is framed for a terminal: banner lines, whole files where forty lines were wanted, a path
repeated on every `grep` hit, and no ceiling anywhere. One `cat` of a generated file can
cost more context than the task was worth, and nothing warns you first.

`fileops` gives that instinct a better instrument. The batching is in the tool, so the
command stops being prose; the output is framed for a context window instead of a screen;
and a byte budget is always in force, so the worst case is a truncation marker rather than
a spent session.

The same call as the shell chain above:

```json
{"specs": [
  {"path": ".specify/extensions.yml", "head": 60},
  {"path": ".specify/init-options.json"},
  {"path": ".specify/feature.json"},
  {"path": "specs/010-file-slice-dedupe/spec.md", "lines": "1-90"},
  {"path": "specs/010-file-slice-dedupe/tasks.md", "grep": "^- \\[ \\]", "max_lines": 30}
]}
```

```
#1 .specify/extensions.yml 1-60/118
1: extensions:
...
#4 specs/010-file-slice-dedupe/spec.md 1-90/412
...
#5 specs/010-file-slice-dedupe/tasks.md 14,19,23/87
14: - [ ] T004 dedupe slices by content hash
19: - [ ] T009 property test for the hash boundary
23: - [ ] T013 document the cache layout
[5 files, 184 lines, 5.9k chars]
```

No banners, no repeated paths, one header per file saying exactly which lines follow, and
a footer that tallies what it cost.

## What it does

| Tool | Behavior |
| --- | --- |
| `read` | Line slices of many files in one call — `cat`, `head`, `tail` and `sed -n` batched. Per spec: `lines` (`12-40,98-120`), `head`/`tail`, `from`/`to` regexes for a section you can name but not number, a `grep` filter with `context`, `max_lines`. Paths may be globs. |
| `grep` | Several regexes at once, grouped by file: the path once, matches with line numbers, caps per file and per call. `mode: counts` or `mode: files` when the matches are not the question. |
| `find` | Listings grouped by directory — `ls`, `find` and `tree`. `depth: 1` is `ls`; `glob`, `kind`, `stat` and `sort` narrow or widen it. |
| `inspect` | Size, line count, kind, date and symlink status for a batch of paths. The cheap call that stops an expensive one. |
| `outline` | The shape of a file without its contents — one line per heading, `fn`, `class`, `interface`, Make target or config section, with its line number. The call that replaces reading the first hundred lines. |
| `extract` | Named values out of JSON, YAML and TOML — `jq '.a.b[0]'` over a batch, without the document. `query` is a dotted path with `[]` fan-out; `keys` lists a level's shape; `depth` summarises instead of expanding. |
| `survey` | What a tree is made of: files, lines and bytes per file type, then the largest files, then the totals. A dozen lines however large the repository. |
| `workspace_inventory` | Read-only sibling workspace and local link inventory in one call: lists `<prefix>-*` directories and reports status/targets for named local symlinks. |

The rules behind them:

* **Nothing takes a single path where a list would do.** One call, one byte budget, one
  footer — eight specs cost one round trip.
* **A byte budget is always in force.** Default 40 000 bytes, `max_bytes` raises it to at
  most 400 000. It cannot be removed, and output never trails off silently: a response that
  ends in `truncated` says what was cut and by which cap.
* **Nothing is unbounded by default.** `grep` renders at most 20 matches per file and 200
  per call, `find` at most 500 entries, and each cap reports when it bites.
* **A path that cannot be read costs one line, not the batch.** `(missing)`,
  `(binary, 24K)`, `(directory)`, `(unreadable: permission denied)` — a bad entry in a
  batch of ten never discards the other nine.
* **Line numbers are on by default**, because a slice you cannot locate is a slice you
  cannot edit against.
* **`.gitignore` and hidden files are skipped while searching and listing**, which is most
  of why this is cheaper than `grep -r`. `no_ignore`/`hidden` override it, `.git` is never
  listed, and a path named outright is always read whatever the filters say.
* **Everything is read-only.** No tool here writes, moves or deletes anything.

[Token-efficient reading](docs/guides/token-efficient-reading.md) covers the batching
patterns, and [Output format](docs/guides/output-format.md) is the reference for every
header, marker and footer.

## Install

```bash
make install          # cargo install --path apps/fileops-mcp
claude mcp add --scope user fileops fileops-mcp
```

Reinstall after changing the code: the registered server runs the installed binary.

`--max-bytes` (or `FILEOPS_MCP_MAX_BYTES`) sets the server-wide default budget; a request
may still name its own.

## Development

```bash
make check    # fmt-check + clippy (warnings denied) + tests
make test
make help
```

All logic lives in `crates/fileops-fs` with its tests; `apps/fileops-mcp` is a thin rmcp
adapter, driven end to end over its own stdio. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

BSD 3-Clause. See [LICENSE](LICENSE).
