---
name: ferret
description:
  Search files by name, content, size and age with the `ferret` CLI, which
  answers from an index instead of reading the tree. Use it for any file or
  content search under an indexed directory, especially queries that combine a
  name or path with content or modification time, and for find(1) or rg commands
  over large trees.
---

# Searching with ferret

`ferret` keeps an index of the directories it has been told about (its roots). A
query under a root reads the index, not the tree, so a selective query costs
about the same on a million files as on a hundred. Outside every root,
`ferret grep` walks the tree, slower than rg; `ferret find` refuses unless given
`-I`; `ferret search` sees only roots.

Check what is indexed once per session:

```sh
ferret roots list
```

## Which command

| You want                                                       | Use                                  |
| -------------------------------------------------------------- | ------------------------------------ |
| Lines matching a pattern, with rg's flags and output           | `ferret grep PATTERN [PATH...]`      |
| Files by name, path, extension, content, size or age, combined | `ferret search ATOM...`              |
| A find(1) command                                              | `ferret find [PATH...] [EXPRESSION]` |

`ferret grep` takes rg's syntax and prints what rg prints, including `--json`,
`-l`, `-c`, `-n`, `-g GLOB` and `-U`. Write it as you would write rg. It
follows ferret's visibility rules, so it lists dot files rg hides; add `--rg`
for exactly the files rg would search. A flag it does not support (`-t`, `-P`, `--sort` and others) exits 2 with
`... is not supported; use rg for this command`; then run rg.

## `ferret search`

Pass one atom per argument. Adjacent atoms must all match.

```sh
ferret search ext:rs text:excludesFile         # .rs files containing the word
ferret search '*.md' path:docs text:"stale index"
ferret search 'src/**/*.rs' 'mtime:<1d'        # changed in the last day
ferret search ext:rs NOT path:tests text:unwrap
ferret search \( ext:ts OR ext:tsx \) text:useEffect
ferret search --json --limit 20 ext:py text:asyncio
```

| Atom                         | Matches                                                                    |
| ---------------------------- | -------------------------------------------------------------------------- |
| `WORD`                       | the name contains WORD; with a `/`, the path does                          |
| `GLOB`                       | `*.rs` the name; `src/**/*.rs` the path's end                              |
| `ext:EXT`, `path:TEXT`       | extension; path substring                                                  |
| `text:WORD`                  | a whole word, or an identifier part: `text:request` finds `requestHandler` |
| `text:"a b"`                 | the words in order, whatever separates them                                |
| `grep:REGEX`                 | a regex over content, case-sensitive, `^`/`$` per line                     |
| `re:REGEX`                   | a regex over the name                                                      |
| `size:>10M`, `mtime:<2h`     | size (k, M, G, T); age (s, m, h, d, w, y)                                  |
| `type:f`, `type:d`, `type:l` | file, directory, symlink                                                   |
| `case:ATOM`                  | ATOM case-sensitively                                                      |

`AND`, `OR`, `NOT`, `(` and `)` are whole arguments. AND binds tighter than OR.

### Traps

- **Quote `<` and `>`.** `mtime:<1d` unquoted is a shell redirect: write
  `'mtime:<1d'`.
- **Quote parentheses** for the shell: `\(` and `\)`.
- **Exit 3 with "pass --scan to read them"** means the query would read too many
  files to verify: a phrase of common words, or `grep:` over a broad set. Narrow
  it with `ext:`, a path or a glob, or use `ferret grep`, or add `--scan` if
  reading them is what you want.
- `text:` matches whole words and identifier parts, never arbitrary substrings:
  `text:quest` does not find `request`. Use `grep:` or `ferret grep` for
  substrings.

### Output and exit status

Plain output is one absolute path per line, in index order (not sorted).
`--json` prints one object per line:

```json
{
  "path": "/abs/path",
  "type": "file",
  "size": 1234,
  "mtime": 1792705627,
  "doc": 109,
  "offset": 512
}
```

`offset` is the byte offset of the first content match, present when content was
verified. A path that is not UTF-8 also carries `path_base64`.

Exit status: 0 printed a row, 1 matched nothing, 2 usage error, 3 runtime error
(no index, refused scan, I/O). `ferret grep` and `ferret find` keep rg's and
find's own exit codes.

## `ferret find`

GNU find syntax, answered from the index:

```sh
ferret find src -name '*.rs' -type f
ferret find . -path '*/tests/*' -name '*.py'
```

It fails, exit 1, when a start or a `-newer` reference is not in the index. Add
`-I` to walk the tree instead.

## Freshness

While the ferret daemon runs, the index follows changes within about a second;
the first search starts it. A warning on stderr,
`no daemon answered; this answer comes from the index as published N min ago`,
means a file written since then may be missing. Re-run the search a few seconds
later, or use `ferret grep` / `ferret find -I` on the path you just wrote to.

Paths ignored by `.gitignore` or ferret's own ignore file are not in the index,
so `search` and a recursive `grep` or `find` never return them. A file named on
the command line is still read. `.ignore` and `.rgignore` do not apply to the
index, so ferret can return files rg would skip; only `grep` outside the
indexed roots honours them.
