# Installation

AeorDB is built from source using the Rust toolchain. There are no external dependencies -- the binary is fully self-contained.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable toolchain, 1.75+)

Install Rust if you don't have it:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Build from Source

Clone the repository and build in release mode:

```bash
git clone https://github.com/AeorDB/aeordb.git
cd aeordb
cargo build --release
```

The binary is located at:

```
target/release/aeordb
```

## Verify the Build

```bash
./target/release/aeordb --help
```

You should see:

```
AeorDB command-line interface

Usage: aeordb <COMMAND>

Commands:
  start            Start the database server
  stress           Run stress tests against a running instance
  emergency-reset  Emergency reset: revoke the current root API key and generate a new one
  export           Export a version as a self-contained .aeordb file
  diff             Create a patch .aeordb containing only the changeset between two versions
  import           Import an export or patch .aeordb file into a target database
  promote          Promote a version hash to HEAD
  gc               Run garbage collection to reclaim unreachable entries
  help             Print this message or the help of the given subcommand(s)
```

## Optional: Add to PATH

```bash
# Copy to a location in your PATH
sudo cp target/release/aeordb /usr/local/bin/aeordb

# Or symlink
sudo ln -s "$(pwd)/target/release/aeordb" /usr/local/bin/aeordb
```

## No External Dependencies

AeorDB does not require:
- A separate database process (it IS the process)
- Runtime libraries or shared objects
- Configuration files (sensible defaults for everything)
- Docker, containers, or orchestration

The single binary is all you need. Point it at a file path and it creates the database on first run.

## Next Steps

- [Quick Start](./quick-start.md) -- start the server and store your first file
