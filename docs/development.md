# Development

## Prerequisites

- The Rust toolchain pinned in `rust-toolchain.toml` (currently 1.98.1)
- [`just`](https://github.com/casey/just)
- Tabularis for testing the installed plugin
- A reachable Harper instance for integration testing

## Build and install locally

Build the debug executable and install it into Tabularis's platform-specific driver directory:

```bash
just dev-install
```

Restart Tabularis, or toggle the plugin in **Settings**, after reinstalling it.

The recipe installs to:

- Linux: `~/.local/share/tabularis/plugins/drivers/harper/`
- macOS: `~/Library/Application Support/tabularis/plugins/drivers/harper/`
- Windows: `%APPDATA%\tabularis\plugins\drivers\harper\`

Use `just uninstall` to remove that development copy.

## Test without Tabularis

The REPL calls the same JSON-RPC dispatch path as the shipped plugin. Credentials come from environment variables and are not printed.

```bash
HARPER_HOST=http://localhost \
HARPER_PORT=9925 \
HARPER_DATABASE=data \
HARPER_USERNAME=HDB_ADMIN \
HARPER_PASSWORD=password \
just repl
```

Example commands:

```text
test_connection
get_databases
get_tables
query SELECT * FROM `data`.`dog`
query UPDATE `data`.`dog` SET `name` = 'Rover' WHERE `id` = 1
```

The REPL also accepts a complete JSON-RPC request on one line.

## Checks

```bash
just fmt
just lint
just test
```

## Source layout

```text
src/
├── lib.rs             shared stdio loop and production dispatch entry points
├── main.rs            thin executable wrapper
├── rpc.rs             method dispatch and response helpers
├── error.rs           plugin error type
├── models.rs          connection parameters and common shapes
├── client.rs          Harper Operations API transport, authentication, and errors
├── handlers/
│   ├── metadata.rs    databases, schemas, tables, columns, indexes, FKs, views, routines
│   ├── query.rs       connection tests, health checks, queries, and explain requests
│   ├── crud.rs        record inserts, updates, and deletes
│   └── ddl.rs         table and attribute changes
└── bin/
    └── test_plugin.rs local REPL for simulating Tabularis calls
```

HTTP, authentication, and Operations API details remain in `client.rs`. Handlers translate between Harper values and Tabularis JSON-RPC shapes; the low-level operation method is not exposed outside the client.

## Publishing

After CI passes, tag the release commit and push the tag. The tag without its `v` prefix must match the version in `.tabularium`.

The release workflow builds Linux x64/arm64, macOS x64/arm64, and Windows x64 bundles, then attaches the zipped plugins and standalone manifest to the GitHub release. Submit the released plugin at [registry.tabularis.dev/submit](https://registry.tabularis.dev/submit) for inclusion in the in-app registry.

## References

- [Tabularis plugin guide](https://github.com/TabularisDB/tabularis/blob/main/plugins/PLUGIN_GUIDE.md)
- [Tabularis manifest schema](https://github.com/TabularisDB/tabularis/blob/main/plugins/manifest.schema.json)
- [Harper Operations API](https://docs.harperdb.io/reference/v5/operations-api/operations)
