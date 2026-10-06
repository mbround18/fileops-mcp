# Working in this repository

`fileops-mcp` exists so that reading a repository costs a round trip and a few hundred
tokens instead of a shell chain and a few thousand. Holding that line while working on the
tool itself is the bar.

## Use the server on itself

This repo registers itself as a project-scope MCP server (`.mcp.json`), so after
`make install` the tools are available here:

* `mcp__fileops__read` — batched line slices, instead of `cat`/`head`/`tail`/`sed -n`.
* `mcp__fileops__grep` — search, instead of `grep -r`.
* `mcp__fileops__find` — listings, instead of `ls`/`find`/`tree`.
* `mcp__fileops__inspect` — sizes and line counts, instead of `ls -l`/`wc -l`/`file`.
* `mcp__fileops__outline` — a file's declarations, instead of reading its first 100 lines.
* `mcp__fileops__extract` — one value out of a JSON/YAML/TOML file, instead of `jq`/`yq`.
* `mcp__fileops__survey` — what the tree is made of, instead of `find | wc -l` pipelines.

Batch them. One `read` with eight specs is the behaviour this repo is arguing for; eight
calls, or one `Bash` command joining eight `cat`s with `echo` separators, is the behaviour
it was written to replace.

## Invariants

The rules in [CONTRIBUTING.md](CONTRIBUTING.md#invariants) are the product, not style
preferences. In short: everything is batched, the budget cannot be removed, truncation is
always reported, a per-path failure never fails the call, nothing walks `.git`, line
numbers are on by default, every tool is read-only, and stdout belongs to the protocol.

A rendering change is not finished until a test asserts the exact text it produces. The
output format *is* the product; `one_call_reads_a_whole_batch_in_order` is the shape those
tests take.

## Layout

* `crates/fileops-fs` — all logic *and* all rendering, with its tests. New behaviour goes
  here.
* `apps/fileops-mcp` — rmcp adapter only: parameter schemas and translation. Keep it thin.

## Checks

```bash
make check     # fmt-check + clippy (warnings denied) + the full test suite
make test      # tests only
make help      # every target
```

Unit tests build real trees in a tempdir; end-to-end tests in `apps/fileops-mcp/tests/`
drive the built binary over its own stdio. Both must stay deterministic — that is why the
walker sorts by file name and dates are rendered without a calendar crate — and neither
may read or write anything outside its own tempdir.

After changing the code, reinstall — the registered server runs the installed binary, not
`target/debug`:

```bash
make install
```

## Docs

User-facing behaviour goes in `docs/guides/*`, developer detail in `CONTRIBUTING.md`, and
`README.md` stays short. A change to the rendered output is not finished until
`docs/guides/output-format.md` matches it.
