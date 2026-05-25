# AeorDB

A **content-addressed file database** that treats your data as a filesystem, not as tables and rows. Store any file at any path, query structured fields with sub-millisecond lookups, and version everything with Git-like snapshots and forks — all from a single binary with no external dependencies.

- **Filesystem, not a schema** — data lives at paths like `/users/alice.json` and `/docs/reports/q1.pdf`. No tables, no migrations.
- **Content-addressed with BLAKE3** — automatic deduplication, integrity verification, and a Merkle tree that makes versioning essentially free.
- **Built-in versioning** — named snapshots, isolated forks, content-stable historical reads, and self-contained `.aeordb` export/import.
- **Native HTTP API** — store with `PUT`, read with `GET`, query with `POST /files/query`. Any HTTP client works; no separate proxy or client library required.
- **Native parsers + WASM plugins** — 8 built-in parsers (text, HTML/XML, PDF, images, audio, video, MS Office, ODF) plus a sandboxed WebAssembly plugin runtime for custom formats and query logic.
- **Lock-free reads** — snapshot double-buffering via `ArcSwap`; queries routinely complete in under a millisecond and never block writers.
- **Embeddable** — single `aeordb` binary, point it at a `.aeordb` file, you have a database.

For the full reference, see [`docs/`](./docs/src/introduction.md) (rendered with `mdbook`).

## Clone

```bash
git clone https://github.com/th317erd/aeordb.git
cd aeordb
```

The repository is a Cargo workspace covering the storage engine (`aeordb-lib`), the CLI / HTTP server (`aeordb-cli`), the plugin SDK (`aeordb-plugin-sdk`), and the bundled parsers (`aeordb-parsers/`).

## Build

AeorDB builds with the stable Rust toolchain (1.75+) and has no external native dependencies.

If you don't have Rust:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then from the repo root:

```bash
cargo build --release
```

The binary lands at `target/release/aeordb`. Smoke-test it:

```bash
./target/release/aeordb start -D /tmp/example.aeordb
```

That starts the HTTP server on `http://127.0.0.1:6830` with self-contained auth, prints a one-time root API key, and creates `/tmp/example.aeordb` if it doesn't exist. `Ctrl-C` to shut down.

For development builds and tests:

```bash
cargo build              # dev build (faster, ~10x slower at runtime)
cargo test               # run the full test suite
cargo test -p aeordb     # just the engine + HTTP tests
```

## Documentation

- **User docs:** [`docs/src/`](./docs/src/) — concepts, API reference, plugin development, operations. Renderable via `mdbook serve docs/`.
- **Onboarding for AI assistants:** [`CLAUDE.md`](./CLAUDE.md) — project-specific instructions loaded into Claude Code sessions.
- **JSON merge-patch (new in 0.9.5):** [`docs/src/api/merge-patch.md`](./docs/src/api/merge-patch.md) — server-side `PATCH /files/<path>` with RFC 7396 semantics and a signed `?depth=N` bound.

## License

AeorDB is source-available under the [Business Source License 1.1](./LICENSE-BSL.md). The short version:

- **Free** for non-production use, internal use under $2M gross annual revenue, and any non-competing use.
- **Commercial license required** for offering AeorDB as a managed service, internal use above the revenue threshold, or building a competing product. Contact <sales@aeor-development.com>.
- **Every release auto-converts to Apache 2.0** four years after its publication date.

See [`LICENSE-BSL.md`](./LICENSE-BSL.md) for the full license text and FAQ.

For commercial inquiries: <sales@aeor-development.com>
Project home: <https://aeordb.com>
