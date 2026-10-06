# Token-efficient reading

Reading files is usually the largest line item in an agent's context budget, and almost
all of it is avoidable. This guide is the set of habits the tools are shaped around.

## 1. One call, many paths

Every tool takes a list. The whole point is that orienting yourself in a repository costs
one round trip:

```json
{"specs": [
  {"path": ".specify/init-options.json"},
  {"path": ".specify/feature.json"},
  {"path": ".specify/memory/constitution.md", "head": 40},
  {"path": "specs/010-file-slice-dedupe/spec.md", "head": 30},
  {"path": "specs/010-file-slice-dedupe/tasks.md", "grep": "^- \\[ \\]", "max_lines": 15}
]}
```

That replaces five `cat`/`head`/`grep` invocations, the `echo '=== … ==='` banners between
them, and — more expensively — the four extra request/response cycles, each of which
re-sends the entire conversation.

The same holds for the others: `grep` takes several `patterns` and several `paths`, `find`
takes several `roots`, `inspect` takes a batch. If you are about to make the same call
twice with a different path, you want one call.

## 2. Ask for less than the file

A whole file is almost never the question. The cheapest spec is the most specific one:

| You want | Spec |
| --- | --- |
| The top of a file | `{"head": 40}` |
| The end of a log | `{"tail": 30}` |
| A known region | `{"lines": "120-180"}` |
| Two regions | `{"lines": "1-40,310-360"}` |
| From a line to the end | `{"lines": "900-"}` |
| Only what matches | `{"grep": "fn ", "context": 2}` |
| A bounded sample of matches | `{"grep": "TODO", "max_lines": 15}` |
| One section of a document | `{"from": "^## Invariants", "to": "^## "}` |
| From a marker to the end | `{"from": "^## Appendix"}` |

`from`/`to` are `sed`'s address ranges (`sed -n '/a/,/b/p'`), and they are how you read a
section you can name but cannot number: `{"path": "CONTRIBUTING.md", "from": "^## Invariants", "to": "^## "}` returns that section and nothing else, and keeps working after
the file grows. Given both, the pair repeats, so `{"from": "^## ", "to": "^## "}` is every
section rather than the first.

`grep` inside a `read` spec is the one worth remembering: it turns "find the handlers in
this 2 000-line file" into fifteen lines with their numbers, which is enough to choose the
slice you actually need.

## 3. Locate before you read

Two cheap calls beat one expensive one. `grep` with `mode: files` says *where* something
is for the price of a few paths; `inspect` says how big the answer would be before you ask
for it:

```json
{"patterns": ["TokenBudget"], "paths": ["crates"], "mode": "files"}
```
```json
{"paths": ["crates/ctx-hook/src/pipeline.rs", "docs/architecture.md"]}
```

`inspect` is the call that stops the expensive one: `792B 100L` means read it, `1.6M
binary` means do not.

## 4. Ask for the shape before the contents

`outline` is the "what is in this file" call. It keeps the line that declares each thing
and throws the rest away, so an unfamiliar 2 000-line file costs forty lines instead of a
hundred that happen to be at the top:

```json
{"paths": ["crates/fileops-fs/src"], "glob": ["**/*.rs"]}
```
```
crates/fileops-fs/src/read.rs (14)
27: pub struct ReadSpec
52: pub struct ReadRequest
116: pub fn read(request: &ReadRequest) -> Result<ReadOutcome>
[…]
```

The line numbers are the point: the next call is a `read` with `lines: "116-180"`, not a
second guess. On a document, `outline` with `levels: 2` is a table of contents, and the
heading it returns is exactly the `from` pattern a `read` needs.

`pattern` makes it work on anything with a convention of its own — `^TASK `, `^- \[ \]`,
`^### T0` — for file types with no built-in pattern.

## 5. Ask a config for the value, not the document

A `package.json`, a lockfile, a CI workflow or a `Cargo.toml` is a document you almost
never want in full. `extract` answers the question:

```json
{"specs": [
  {"path": "package.json", "query": "scripts"},
  {"path": "Cargo.toml", "query": "workspace.dependencies", "depth": 1},
  {"path": ".github/workflows/ci.yml", "query": "jobs[].steps[].run"}
]}
```

`[]` fans out over an array, and the paths that come back carry the real index, so the
answer doubles as the query for the next call. Against a document you have not seen,
`keys: true` first — one line per child with its type and size — then a query for the
branch that matters. `depth: 1` is the middle ground: a line per key with `{3 keys}` or
`[12]` where a subtree would have been.

## 6. Start with the survey, not the listing

`find` on an unfamiliar repository is the wrong first call: it costs a line per path to
tell you something a dozen lines could. `survey` aggregates the same walk:

```json
{"paths": ["."], "exclude": ["**/target/**"]}
```
```
rs 17 files 207K 6.4kL
md 5 files 28K 659L
toml 3 files 2.1K 75L
largest:
apps/fileops-mcp/src/server.rs 24K 607L
[28 files in 11 dirs, 271K, 8.5kL]
```

That is the language, the size, and where the weight sits, in one bounded call — and the
`largest:` table is usually the list of files worth outlining next. On a very large tree
`lines: false` skips opening files altogether.

## 7. Narrow the walk, not the output

`grep` and `find` honour `.gitignore` and skip hidden files by default, and never enter
`.git`. That — not the formatting — is most of why they are cheaper than `grep -r`. Narrow
further with the walk, before anything is read:

* `glob: ["**/*.rs"]` / `exclude: ["**/target/**", "**/*.lock"]`
* `depth: 1` on `find` is `ls`; `depth: 2` is a readable `tree`
* `kind: "dir"` to see the shape of a tree without its files
* `no_ignore: true` and `hidden: true` when you genuinely need generated or dotted files

A path you name outright is always read, whatever the filters say — so a `glob` never
hides the one file you asked for.

## 8. Let the budget do the worrying

Every response has a byte ceiling: 40 000 by default, up to 400 000 with `max_bytes`. It
cannot be switched off, so the worst case of a careless call is a truncation marker rather
than a spent session:

```
#1 generated.json 1-18/40000 +39982 lines cut
#2 notes.md (skipped: byte budget reached)
[2 files, 18 lines, 281 chars, 1 truncated, 1 skipped: raise max_bytes or narrow the specs]
```

Lower it on purpose when you are sampling — `max_bytes: 2000` across ten specs is a
reliable way to see a little of everything. Raise it only when you know the answer is
large and you need all of it. Caps that are not the budget behave the same way: `grep`
shows 20 matches per file and 200 per call, `find` 500 entries, and each one says when it
bit. `mode: counts` and `mode: files` are one line per file, so those two caps do not
apply to them — the tally is always the whole tally.

## 9. Keep the line numbers

`number: true` is the default because a slice without line numbers cannot be edited
against — you would have to read the file again to find out where you were. Turn it off
only when the text itself is the deliverable (a file being copied verbatim), and
`raw: true` only when trailing whitespace matters.

## 10. What a batch should look like

Orienting in an unfamiliar repository, in four calls:

```json
{"paths": ["."], "exclude": ["**/target/**", "**/node_modules/**"]}
```
```json
{"patterns": ["fn main", "#\\[tokio::main\\]"], "paths": ["."], "glob": ["**/*.rs"], "mode": "files"}
```
```json
{"paths": ["README.md", "apps"], "levels": 2, "glob": ["**/*.rs", "**/*.md"]}
```
```json
{"specs": [
  {"path": "README.md", "head": 60},
  {"path": "Cargo.toml"},
  {"path": "apps/thing/src/main.rs", "max_lines": 80},
  {"path": "CONTRIBUTING.md", "grep": "^## ", "context": 0}
]}
```

Survey, then location, then outline, then content — and never a whole file where a heading
list would have done.
