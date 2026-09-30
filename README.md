# Harper Driver — Tabularis Plugin

Driver plugin for [Tabularis](https://github.com/TabularisDB/tabularis).
Generated with `@tabularis/create-plugin`.

## Getting started

```bash
just dev-install       # build + install into ~/.local/share/tabularis/plugins/drivers/harper
```

Then open Tabularis — the Harper driver appears in the connection picker.

## What's implemented

| Method | Status | Notes |
|--------|--------|-------|
| `test_connection` | implemented | authenticates with `describe_all` so invalid credentials fail |
| `ping` | implemented | uses Harper's lightweight `/health` endpoint |
| `get_databases`, `get_tables`, `get_columns`, `get_indexes` | implemented | uses Harper describe operations; supports v4 `hash_attribute` and v5 `primary_key` metadata |
| `get_schemas`, `get_foreign_keys` | implemented | return `[]`; Harper databases are selected as databases and the driver does not expose foreign keys |
| `get_views*`, `get_routines*` | stub | return empty results while their capabilities remain disabled |
| `create_view`, `alter_view`, `drop_view` | `-32601` | not implemented — flip `capabilities.views` once these are wired |
| `execute_query` | implemented | accepts one `SELECT` statement and converts Harper JSON rows to the Tabularis grid shape |
| `explain_query` | `-32601` | Harper does not currently expose an explain operation through this driver |
| `insert_record`, `update_record`, `delete_record` | `-32601` | the initial driver is deliberately read-only |
| DDL generators | `-32601` | implement when table management is enabled |
| Schema snapshot and batch metadata methods | stub | return empty results; the standard per-table metadata path is implemented |

## Layout

```
src/
├── lib.rs             shared stdio loop + production dispatch entry points
├── main.rs            thin executable wrapper
├── rpc.rs             method dispatch + response helpers
├── error.rs           plugin error type
├── models.rs          ConnectionParams + common shapes
├── client.rs          Harper Operations API transport, auth and errors
├── handlers/
│   ├── metadata.rs    databases, schemas, tables, columns, indexes, FKs, views, routines
│   ├── query.rs       test_connection, ping, execute_query, explain_query
│   ├── crud.rs        insert_record, update_record, delete_record
│   └── ddl.rs         CREATE/ALTER/DROP generators
├── utils/
│   ├── identifiers.rs quote_identifier(name) + tests
│   └── pagination.rs  paginate(query, page, size) + tests
└── bin/
    └── test_plugin.rs local REPL for simulating Tabularis calls
```

HTTP, authentication and Operations API details stay in `client.rs`. Handlers only translate between Harper values and Tabularis JSON-RPC shapes; the low-level operation method is not exposed outside the client.

## Testing without Tabularis

```bash
HARPER_HOST=localhost \
HARPER_PORT=9925 \
HARPER_DATABASE=data \
HARPER_USERNAME=HDB_ADMIN \
HARPER_PASSWORD=password \
just repl

# > test_connection
# > get_databases
# > get_tables
# > query SELECT * FROM data.dog
```

The REPL also accepts a complete JSON-RPC request on one line. It calls the same production dispatch path as the shipped plugin; credentials are read from the environment and are not printed.

## Publishing

Tag a commit `v0.1.0` and push — the included GitHub Actions workflow builds for Linux (x64/arm64), macOS (x64/arm64), and Windows (x64), then attaches the zipped plugin bundles to the release. Submit a PR to `plugins/registry.json` in the Tabularis repo to publish to the in-app registry.

## References

- [Plugin guide](https://github.com/TabularisDB/tabularis/blob/main/plugins/PLUGIN_GUIDE.md)
- [Manifest schema](https://github.com/TabularisDB/tabularis/blob/main/plugins/manifest.schema.json)
- [Tabularis repo](https://github.com/TabularisDB/tabularis)

## License

Apache-2.0
