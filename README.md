<p align="center">
  <a href="https://www.harper.fast/">
    <img src="./harper.png" alt="Harper" width="144">
  </a>
</p>

<h1 align="center">Harper for Tabularis</h1>

<p align="center">
  Explore, query, and edit <a href="https://www.harper.fast/">Harper</a> data from
  <a href="https://github.com/TabularisDB/tabularis">Tabularis</a>.
</p>

This driver connects Tabularis to a Harper instance through Harper's HTTP Operations API. It brings Harper databases and tables into the Tabularis explorer while preserving Harper's flexible document model.

## Why Harper?

Harper is an all-in-one backend that combines a database, caching, application hosting, and messaging in a single runtime. Its database provides ACID-compliant storage, flexible schemas, automatic and explicit indexing, and access to the same data through several interfaces—including SQL, REST, and Harper applications.

This plugin focuses on the database administration and ad-hoc querying experience. It does not attempt to configure Harper applications, REST resources, messaging, replication, or AI models from Tabularis.

## Features

- Browse Harper databases, tables, attributes, primary keys, and per-attribute indexes.
- Inspect heterogeneous documents in Tabularis's data grid, even when records have different fields.
- Run one Harper SQL `SELECT`, `INSERT`, `UPDATE`, or `DELETE` statement at a time.
- Insert, update, and delete records directly from the grid.
- Create and drop tables.
- Add attributes to dynamic tables and remove attributes when Harper can safely remove their stored values.
- Connect over HTTP for trusted local development or HTTPS with publicly trusted certificates.
- Work with Harper v4 and v5 metadata shapes.

## Current boundaries

Tabularis can display Harper indexes, but it cannot create or drop named, unique, or compound indexes through this plugin. Views, foreign keys, routines, SQL `EXPLAIN`, full column alterations, custom certificate authorities, and client certificates are not currently exposed.

Harper schema-defined tables have additional safeguards: adding a column is limited to `ANY`, and dropping a declared attribute is rejected when Harper would retain its stored values. Grid updates and deletes require a supported scalar primary key. See [Capabilities and limitations](docs/capabilities.md) for the complete compatibility notes.

## Install

Install **Harper** from **Settings → Available Plugins** in Tabularis. Plugin releases are also available from this repository's [Releases page](../../releases/latest).

To connect, provide the Harper host, port, username, and password. Port `9925` is the default. Selecting a database is optional for browsing; choosing one explicitly is recommended before making schema or data changes.

For a local Harper instance using plain HTTP, use an explicit `http://localhost` host or set SSL mode to **Disabled**. Only disable TLS on a trusted network because Harper credentials are sent with the request.

## Querying

Harper SQL uses `database.table` names. Quote identifiers with backticks when they contain special characters or collide with a reserved word:

```sql
SELECT *
FROM `data`.`animal`
ORDER BY `name`
LIMIT 100;
```

Write queries must always qualify the target with its database so the plugin cannot apply a change to an unintended database.

Harper recommends SQL for investigation and administration rather than performance-sensitive production access. Applications should use Harper's native application, REST, or NoSQL interfaces where appropriate.

## How it works

Tabularis starts the `harper` executable and communicates with it over JSON-RPC on standard input and output. The plugin translates those requests into authenticated HTTP calls to Harper's Operations API; no separate Rust SDK is required.

## Documentation

- [Capabilities and limitations](docs/capabilities.md)
- [Development and local installation](docs/development.md)
- [Harper documentation](https://docs.harperdb.io/)
- [Tabularis plugin registry](https://registry.tabularis.dev/plugins)

## License

Licensed under the [Apache License 2.0](LICENSE).
