# Output format

Every tool returns one block of text plus the same information as structured JSON. The
text is what costs context, so it is designed to be read rather than parsed: each file is
named once, each line says where it came from, and the last line says what the call cost.

Every example below is real output from a tree containing `src/big.txt` (100 lines),
`src/lib.rs`, `src/blob.bin` (binary), `src/link.txt` (a symlink to `big.txt`) and
`docs/guide.md`.

## Shared rules

* **Paths are relative to `cwd`**, with no `./` prefix. A path outside `cwd` is printed in
  full.
* **The last line is a footer** in `[…]` — counts of what was emitted, not what exists.
  When something was cut, the footer says so and the structured outcome sets `truncated`.
* **A path that cannot be read becomes a note**, never an error: `(missing)`,
  `(directory)`, `(binary, 9B)`, `(unreadable: permission denied)`.
* **Sizes** are compact: `792B`, `4.2K`, `1.6M`. Character counts in footers use `6.1k`.
* **Dates** are `YYYY-MM-DD`, from the modification time.

## `read`

```
#1 src/big.txt 1-2/100
1: line 1
2: line 2
#2 src/big.txt 50-51/100
50: line 50
51: line 51
#3 src/lib.rs 1-4/4
1: alpha
2: beta needle
3: gamma
4: needle again
#4 src/blob.bin (binary, 9B)
#5 nope.txt (missing)
[5 files, 8 lines, 212 chars]
```

The header is `#<spec index> <path> <spans>/<total lines>`:

* `#3` is the position of the spec in the request, so a batch can be matched up with what
  was asked for even when a path appears twice.
* `1-2` is the slice being shown, `12-40,98-120` when it is several, `/100` the file's
  real length. A header with no spans is a status line for a path that was not read.
* A spec selected by `from`/`to` renders like any other slice — the header names the line
  numbers the section turned out to occupy (`3-6/7`), so it can be read again or edited
  against without re-deriving them.
* Body lines are `<line number>: <text>`. With `number: false` the numbers are dropped and
  a `...` line marks each gap between spans instead.

Markers:

| Marker | Meaning |
| --- | --- |
| `+82 lines cut` | A cap (`max_lines` or the byte budget) stopped the slice early. |
| `(skipped: byte budget reached)` | The budget ran out before this spec was reached. |
| `(binary, 9B)` | Sniffed as binary; not rendered. |
| `(missing)`, `(directory)`, `(unreadable: …)` | The path could not be read as text. |
| `(no matches)` | A spec's `grep` filter matched nothing. |

A call that hits its ceiling still delivers everything it could, and says what it cut:

```
#1 src/big.txt 1-18/100 +82 lines cut
1: line 1
…
18: line 18
#2 src/lib.rs (skipped: byte budget reached)
[2 files, 18 lines, 281 chars, 1 truncated, 1 skipped: raise max_bytes or narrow the specs]
```

## `grep`

The path is printed once per file, with its match count:

```
docs/guide.md (1)
1- title
2: body needle
src/lib.rs (2)
1- alpha
2: beta needle
3- gamma
4: needle again
[3 matches in 2 files, 3 searched]
```

* `:` marks a matching line, `-` a context line (`context: 1` above). This is `grep`'s own
  convention, minus the repeated path on every hit.
* `src/lib.rs (2)` is the count shown; `src/lib.rs (3 of 100)` means a cap cut the rest.
* `searched` counts the files opened, which is how to tell an empty result from a walk
  that never reached the right directory.

`mode: counts` keeps the tally and drops the lines; `mode: files` keeps only the paths —
the cheap first call when the next step is a `read`. Both render one line per file, so the
match caps do not apply to them and the counts are complete:

```
docs/guide.md 1          docs/guide.md
src/lib.rs 2             src/lib.rs
```

## `find`

Entries are grouped so each directory is named once:

```
./
  docs/ src/
docs/
  guide.md
src/
  big.txt blob.bin lib.rs link.txt
[7 of 7 entries (2 dirs)]
```

* Directory names end in `/`. Names wrap at 96 columns.
* The footer is `[<shown> of <found> entries (<dirs> dirs)]`; `shown < found` means `limit`
  cut the listing.
* `stat: true`, `flat: true`, or sorting by anything but path renders one entry per line:

```
docs/guide.md 18B 2026-10-06
src/big.txt 792B 2026-10-06
[5 of 5 entries (0 dirs)]
```

## `inspect`

One line per path, with only the facts that apply:

```
src/big.txt 792B 100L 2026-10-06
src/blob.bin 9B binary 2026-10-06
src/link.txt 792B 100L symlink 2026-10-06
src/ 4 entries
nope.txt (missing)
[5 paths, 3 files, 1.6K, 1 missing]
```

* `100L` is the line count, omitted for binaries and when `lines: false`.
* `symlink` marks a link; the size and line count are the target's.
* A directory reports how many entries it holds instead of a size.
* The footer totals the bytes of the files it could stat.

## Structured output

Alongside the text, each tool returns the same data as JSON: `read` gives
`files[] {index, path, status, detail, total_lines, shown, dropped}` with `lines_shown`,
`truncated` and `skipped`; `grep` gives `files[] {path, matches, shown, lines}` with
`matches`, `searched` and `truncated`; `find` gives `entries[] {path, dir, bytes,
modified}` with `found` and `truncated`; `inspect` gives `paths[] {path, status, detail,
bytes, lines, entries, modified, symlink}` with `total_bytes` and `missing`. Read the text
— the JSON is there for callers that need to branch on a count.
