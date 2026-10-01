# Capabilities and limitations

This document describes how the Harper driver maps Tabularis features to Harper's Operations API. The shorter, user-facing overview lives in the [project README](../README.md).

## Tabularis operations

| Method or feature | Status | Notes |
|---|---|---|
| `test_connection` | Supported | Authenticates with `user_info`, so invalid credentials fail the connection test. |
| `ping` | Supported | Uses Harper's lightweight `/health` endpoint. |
| `get_databases`, `get_tables`, `get_columns`, `get_indexes` | Supported | Uses Harper describe operations and accepts v4 `hash_attribute` and v5 `primary_key` metadata. |
| `get_schemas`, `get_foreign_keys` | Empty by design | Harper databases appear as databases in Tabularis, and the driver does not expose foreign keys. |
| `get_views*`, `get_routines*` | Empty by design | Their capabilities are disabled in the plugin manifest. |
| `create_view`, `alter_view`, `drop_view` | Unsupported | Returns JSON-RPC `-32601`. |
| `execute_query` | Supported | Accepts one `SELECT`, `INSERT`, `UPDATE`, or `DELETE`. Supported Tabularis DDL is translated to native Harper operations. |
| `explain_query` | Unsupported | Harper does not expose an explain operation through this driver. |
| `insert_record`, `update_record`, `delete_record` | Supported | Uses native Harper operations. Update and delete verify the table's primary key before writing. |
| Create and drop table | Supported | Maps Tabularis SQL previews to Harper schema operations. |
| Add and drop attribute | Partially supported | See the schema rules below. |
| Alter attribute, named index mutation, and foreign keys | Unsupported | Harper has no matching atomic Operations API operation. The plugin fails explicitly instead of approximating the requested mutation. |
| Schema snapshot and batch metadata | Supported | Loads a database description once and returns Tabularis's current metadata shapes. |

## SQL and result sets

SQL `INSERT`, `UPDATE`, and `DELETE` targets must be qualified as `database.table`. This prevents Harper from resolving an unqualified write against a different database from the active Tabularis context.

Queries without their own top-level `LIMIT` are fetched from Harper one page at a time. `total_count` is a monotonic lower bound until the final page because Harper SQL does not expose an efficient count alongside arbitrary query results.

Individual pages and Tabularis's **All** mode are limited to 10,000 rows. **All** sets `truncated: true` when more rows exist. The HTTP transport also enforces a 16 MiB response ceiling.

Harper documents can have different fields in every row. The plugin discovers the fields in the visible page and builds a rectangular result for Tabularis. That result is limited to 1,000 columns and 1,000,000 empty padding cells. Paged queries return a precise error when the result is too sparse; **All** mode returns the largest complete prefix and marks it as truncated.

## Schema behavior

- Table creation uses Harper's schema-defined attributes. Current Harper versions enforce declared types and nullability, and inserts do not automatically create undeclared attributes.
- `VARCHAR` maps to Harper's `String` type and is previewed as `TEXT`; Harper does not enforce a character-length limit for it.
- Adding a column supports `ANY` only. Harper's `create_attribute` operation has no type, nullability, or default input, so the plugin rejects typed additions rather than reporting a type Harper did not enforce.
- Dropping an attribute is rejected for schema-defined tables because current Harper versions retain the stored property values. Attribute drops remain available on dynamic tables when Harper can purge the data.
- Harper manages indexes per attribute. Tabularis displays them, but the Operations API does not support creating or dropping user-named, unique, or compound indexes through this driver.
- Foreign keys, views, routines, and SQL `EXPLAIN` are not advertised because Harper does not expose matching enforced semantics through this driver.
- `INTEGER`, `LONG`, and `ANY` primary keys created with auto increment are optional on insert so Harper can generate the key. Other primary-key types require a value.
- Grid updates and deletes support non-empty string and numeric primary keys. Composite, object, array, boolean, null, and empty-string keys are rejected before a request is sent, as are numeric keys outside JavaScript's safe integer range; use a string key for larger values.
- When a connection does not select a database, metadata reads use the first configured database. Row edits and destructive DDL require an explicit database when more than one is available.

## TLS

The plugin accepts Tabularis's PostgreSQL- and MySQL-style TLS mode names. A bare host defaults to HTTPS, except `localhost`, which defaults to HTTP for local development. Loopback IP addresses still default to HTTPS because Tabularis also uses them for tunneled remote connections.

Use an explicit `http://` URL or select **Disabled** for another intentionally unencrypted connection. Do this only on a trusted network because Basic-auth credentials are sent unencrypted. Preferred, required, and verification modes force HTTPS, including on `localhost`; an explicit `http://` host is rejected in those modes.

Custom CA, client certificate, and client key files are rejected until the plugin can apply them to its Rust TLS client. They are never silently ignored, so self-signed HTTPS endpoints currently require a publicly trusted certificate.
