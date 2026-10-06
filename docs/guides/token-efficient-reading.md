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

## 4. Narrow the walk, not the output

`grep` and `find` honour `.gitignore` and skip hidden files by default, and never enter
`.git`. That — not the formatting — is most of why they are cheaper than `grep -r`. Narrow
further with the walk, before anything is read:

* `glob: ["**/*.rs"]` / `exclude: ["**/target/**", "**/*.lock"]`
* `depth: 1` on `find` is `ls`; `depth: 2` is a readable `tree`
* `kind: "dir"` to see the shape of a tree without its files
* `no_ignore: true` and `hidden: true` when you genuinely need generated or dotted files

A path you name outright is always read, whatever the filters say — so a `glob` never
hides the one file you asked for.

## 5. Let the budget do the worrying

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

## 6. Keep the line numbers

`number: true` is the default because a slice without line numbers cannot be edited
against — you would have to read the file again to find out where you were. Turn it off
only when the text itself is the deliverable (a file being copied verbatim), and
`raw: true` only when trailing whitespace matters.

## 7. What a batch should look like

Orienting in an unfamiliar repository, in three calls:

```json
{"roots": ["."], "depth": 2, "kind": "dir"}
```
```json
{"patterns": ["fn main", "#\\[tokio::main\\]"], "paths": ["."], "glob": ["**/*.rs"], "mode": "files"}
```
```json
{"specs": [
  {"path": "README.md", "head": 60},
  {"path": "Cargo.toml"},
  {"path": "apps/thing/src/main.rs", "max_lines": 80},
  {"path": "CONTRIBUTING.md", "grep": "^## ", "context": 0}
]}
```

Shape, then location, then content — and never a whole file where a heading list would
have done.
