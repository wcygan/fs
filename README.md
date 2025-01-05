# fs

`fs`, short for "File Search," is a tool that searches for files in a directory using a breadth-first search (BFS). It optionally respects `.gitignore` rules, lets you filter by file extensions, controls maximum search depth, and can include or exclude hidden files.

## Quickstart

```bash
cargo install --git https://github.com/wcygan/fs
```

## Usage

Search in the current directory (recursively) for all files:

```bash
fs
```

Search only files with .rs or .toml extensions:

```bash
fs --extensions rs,toml
```

Search for files matching abc* in the ~/Download directory, up to 2 levels deep:

```bash
fs ~/Downloads --pattern "IMG" --max-depth 2
```

Include hidden files and ignore .gitignore:

```bash
fs --show-hidden --include-gitignored
```

## Help

```bash
A file system search tool that supports .gitignore

Usage: fs [OPTIONS] [ROOT_PATH]

Arguments:
  [ROOT_PATH]  The root directory to start the search from [default: .]

Options:
  -p, --pattern <PATTERN>
          Search pattern to match against file names (use '*' wildcard; naive only)
          [default: *]

  -m, --max-depth <MAX_DEPTH>
          Maximum depth to search (unlimited if not provided)

  -e, --extensions <EXTENSIONS>...
          Only search files with these extensions (comma-separated)

  -H, --show-hidden
          Show hidden files and directories (Unix: name starts with '.', Windows: hidden attribute set) [default: false]

      --include-gitignored
          By default, paths matching .gitignore are skipped. If this option is set, they are included. [default: false]

  -h, --help
          Print help

  -V, --version
          Print version
```