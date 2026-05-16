# vorto

A modal terminal text editor written in Rust, with tree-sitter syntax
highlighting and Language Server Protocol support.

## Features

- Modal editing (normal / insert / visual / command) inspired by Vim
- Tree-sitter based syntax highlighting; grammars are built and installed
  on demand via `vorto grammar install`
- Language Server Protocol client: diagnostics, hover, completion, code
  actions, goto definition
- Fuzzy file / buffer / symbol finder
- Configurable keymap, theme, and per-language indent settings via TOML
- VCS-aware gutter (git)
- System clipboard integration

## Install

From crates.io:

```sh
cargo install vorto
```

From source:

```sh
git clone https://github.com/shka-k/vorto.git
cd vorto
make install            # installs to ~/.local/bin
# or
cargo build --release   # binary at target/release/vorto
```

Requires a Rust toolchain (edition 2024). A C compiler is needed when
building tree-sitter grammars.

## Usage

```sh
vorto [FILE]
vorto -h | --help
vorto -V | --version
```

### Installing grammars

vorto ships with recipes for common languages but does not bundle the
compiled libraries. Install the ones you need:

```sh
vorto grammar list                  # show built-in recipes + install status
vorto grammar install rust python   # build and install specific grammars
vorto grammar install --all         # install everything
vorto grammar install-queries rust  # refresh highlight queries only
vorto grammar remove rust
```

Built-in recipes include: rust, python, go, javascript, typescript, tsx,
toml, kotlin, c, cpp, java, bash, json, yaml, markdown, html, css, lua,
ruby, zig.

## Configuration

Configuration lives under `$XDG_CONFIG_HOME/vorto/` (typically
`~/.config/vorto/`):

- `config.toml` — editor settings, keymap, theme, language overrides
- `grammars/` — installed tree-sitter `.so` libraries
- `queries/<lang>/` — installed `highlights.scm`, `indents.scm`, etc.

## License

Licensed under either of

- MIT License ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
