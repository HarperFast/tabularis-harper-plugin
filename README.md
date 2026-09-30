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
| `execute_query` | implemented | accepts one `SELECT`, `INSERT`, `UPDATE`, or `DELETE`; supported Tabularis DDL is translated to native Harper operations |
| `explain_query` | `-32601` | Harper does not currently expose an explain operation through this driver |
| `insert_record`, `update_record`, `delete_record` | implemented | uses native Harper operations; update/delete verify the table's real primary key first |
| Create/drop table, add/drop attribute | implemented | maps Tabularis SQL previews to Harper schema operations |
| Alter attribute, named index mutation, foreign keys | unsupported | Harper has no atomic Operations API equivalent; the plugin returns a precise error instead of approximating the mutation |
| Schema snapshot and batch metadata methods | implemented | loads a database description once and returns Tabularis's current metadata shapes |

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
└── bin/
    └── test_plugin.rs local REPL for simulating Tabularis calls
```

HTTP, authentication and Operations API details stay in `client.rs`. Handlers only translate between Harper values and Tabularis JSON-RPC shapes; the low-level operation method is not exposed outside the client.

## Testing without Tabularis

```bash
HARPER_HOST=http://localhost \
HARPER_PORT=9925 \
HARPER_DATABASE=data \
HARPER_USERNAME=HDB_ADMIN \
HARPER_PASSWORD=password \
just repl

# > test_connection
# > get_databases
# > get_tables
# > query SELECT * FROM data.dog
# > query UPDATE data.dog SET name = 'Rover' WHERE id = 1
```

The REPL also accepts a complete JSON-RPC request on one line. It calls the same production dispatch path as the shipped plugin; credentials are read from the environment and are not printed.

Queries without their own top-level `LIMIT` are fetched from Harper one page at a time. `total_count` is a monotonic lower bound until the final page because Harper SQL does not expose an efficient count alongside arbitrary query results. Individual pages and Tabularis's **All** mode are limited to 10,000 rows; **All** sets `truncated: true` when more rows exist. The transport also enforces a 16 MiB response ceiling.

SQL `INSERT`, `UPDATE`, and `DELETE` targets must be qualified as `database.table`. This prevents Harper from resolving an unqualified write against a different database than the active Tabularis context.

Harper documents may have different fields in every row. The plugin discovers all fields on the visible page, but bounds the resulting rectangular grid to 1,000 columns and 1,000,000 empty padding cells. Paged queries return a precise error when that shape is too sparse; **All** mode returns the largest complete prefix and marks it truncated.

## Harper-specific behavior

- Table creation uses Harper's schema-defined attributes. Declared types and nullability are enforced by current Harper versions, and inserts do not create undeclared attributes automatically.
- `VARCHAR` input maps to Harper's `String` type and is previewed as `TEXT`; Harper does not enforce a character-length limit for it.
- Dropping an attribute is rejected for schema-defined tables because current Harper would retain the stored property values. Dynamic-table attribute drops remain available when Harper can purge the data.
- Adding a column supports `ANY` only. Harper's `create_attribute` operation has no type/nullability/default input, so the plugin rejects typed additions rather than reporting a type it did not enforce.
- Harper manages per-attribute indexes. They are shown accurately, but the Operations API does not support creating/dropping user-named, unique, or compound indexes.
- Foreign keys, views, routines, and SQL EXPLAIN are not advertised because Harper does not expose matching enforced semantics through this driver.
- Numeric primary keys are auto-assigned by Harper; string/ID-style primary keys receive generated IDs when omitted.
- Grid updates and deletes support non-empty string and numeric primary keys. Composite, object, array, boolean, null, and empty-string keys are rejected before any request is sent.
- When Tabularis omits a database selection, metadata reads use the first database configured on the connection. Row edits and destructive DDL require an explicit database when the connection lists more than one, preventing a write from being guessed into the wrong database.

## TLS

The plugin accepts Tabularis's PostgreSQL- and MySQL-style TLS mode names. A bare host defaults to HTTPS, except loopback names and addresses such as `localhost`, `127.0.0.1`, and `::1`, which default to HTTP for local Harper development. Use an explicit `http://` URL or select Disabled for any other intentionally unencrypted connection, and only on a trusted network because Basic-auth credentials will be sent unencrypted. Preferred/required/verification modes force HTTPS, including on loopback, and an explicit `http://` host is rejected for those modes. Custom CA, client certificate, and client key files are rejected until the plugin can apply them to its Rust TLS client; they are never silently ignored, so self-signed HTTPS endpoints require a publicly trusted certificate for now.

## Publishing

Tag a commit `v0.1.0` and push — the included GitHub Actions workflow builds for Linux (x64/arm64), macOS (x64/arm64), and Windows (x64), then attaches the zipped plugin bundles to the release. Submit a PR to `plugins/registry.json` in the Tabularis repo to publish to the in-app registry.

## References

- [Plugin guide](https://github.com/TabularisDB/tabularis/blob/main/plugins/PLUGIN_GUIDE.md)
- [Manifest schema](https://github.com/TabularisDB/tabularis/blob/main/plugins/manifest.schema.json)
- [Tabularis repo](https://github.com/TabularisDB/tabularis)

## License

Apache-2.0
