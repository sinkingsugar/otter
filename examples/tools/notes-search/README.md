# Notes search tool

A tool a host loads at run time (`otter_lang::Tool`, [docs/embedding.md §6](../../../docs/embedding.md#6-tools)) that searches a SQLite notes database through its FTS5 index with `Db.Query` (`crates/otter-sqlite`), read-only.

| File | Tool | What it does |
|---|---|---|
| `search-notes.otr` | `search_notes` | Full-text search: the best matches (FTS5 `rank`), each with a snippet whose matched words are in `[brackets]`. Arguments: `query` (FTS5 syntax: words, `"phrases"`, `OR`, `NOT`, `prefix*`) and `limit` (1 to 50, default 10). |

The database holds an FTS5 table `notes_fts` with `title` and `body` columns:

```sql
CREATE VIRTUAL TABLE notes_fts USING fts5(title, body);
```

Its path is the host's setting `env.database`, relative to the call's working directory, or `notes.db` there. A call is confined to its working directory by default; to read a database elsewhere, the host inserts an `FsAccess` with a read grant for that file (`FsAccess::within(work)?.grant_read(&db)?`, [embedding.md §3](../../../docs/embedding.md#3-the-catalog-and-scripts)) and passes the absolute path in `env.database`. The database may be in WAL mode and written by another process while the tool reads it.

## Running it

```sh
cargo build --release -p otter-cli
cd dir-with-notes-db
otter tool path/to/examples/tools/notes-search/search-notes.otr                          # the definition, as JSON
otter tool path/to/examples/tools/notes-search/search-notes.otr '{"query": "green tea"}'
```

## Tests

`crates/otter-cli/tests/cli.rs` (`mod sqlite`) builds a WAL notes database, keeps a writer connection open, and runs the tool through `otter tool`.
