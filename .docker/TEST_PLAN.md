# db_client integration test plan

Scope: prove `db_client`'s providers actually connect to and operate against
real servers, not just against mocked/in-memory state. Every test below runs
against `docker-compose.integration.yml` (see repo root) and is gated behind
`#[ignore]` + an env var, per the existing convention in
`crates/db_client/src/*.rs`.

Run everything:

```
docker compose -f docker-compose.integration.yml up -d

MYSQL_TEST_URL=mysql://root:toor@127.0.0.1:13306/instruments \
POSTGRES_TEST_URL=postgres://root:toor@127.0.0.1:15432/test_db \
CASSANDRA_TEST_URL=cassandra://none:none@127.0.0.1:19042 \
MONGO_TEST_URL=mongodb://127.0.0.1:27018 \
REDIS_TEST_URL=redis://127.0.0.1:16379 \
AEROSPIKE_TEST_URL=aerospike://127.0.0.1:13000 \
CLICKHOUSE_TEST_URL=clickhouse://127.0.0.1:18123 \
cargo test -p db_client --lib integration_tests -- --ignored --test-threads=1

docker compose -f docker-compose.integration.yml down
```

## MySQL (`crates/db_client/src/mysql.rs`)

| Test | Proves |
|---|---|
| `test_ping` | Connection + auth actually work |
| `test_list_databases` | `information_schema` discovery |
| `test_list_tables` | Table listing within a database |
| `test_describe_table` | Column introspection |
| `test_get_database_ddl` | `SHOW CREATE DATABASE` reconstruction |
| `test_execute_select_query` | Query execution + row/column decode |
| `test_execute_show_databases` / `test_execute_show_create_table` | Non-`SELECT` statement forms the grid still has to render |
| `test_null_cells_decode_as_none` | NULL must not decode as `0`/`""` |
| `test_unbounded_select_is_bounded` | A full-table scan on `instruments.company_owners` (seeded to 1000 rows by `.docker/mysql/init.sql`) stops at `MAX_RESULT_ROWS` (500), not the freeze-prone unbounded fetch |
| `test_list_users_populates_grants` | `SHOW GRANTS` populates the user list |
| `test_connect_from_non_tokio_executor` | The GPUI-spawned path doesn't panic with "no reactor running" |
| `test_with_cte_returns_rows` | `WITH ... AS (...)` query shape |
| `test_list_foreign_keys_finds_a_declared_fk` | FK introspection via `information_schema.KEY_COLUMN_USAGE` |
| `test_list_check_constraints_finds_a_declared_check` | CHECK constraint introspection |
| `test_list_procedures_finds_a_created_procedure_and_function` | Stored procedure and function listing, correctly classified by `ProcedureKind` |
| `test_list_triggers_finds_a_created_trigger` | Trigger introspection (name/event/timing/table) |
| `test_list_events_finds_a_created_event` | Scheduled event introspection |
| `test_list_users_finds_the_connected_root_user` | `list_users` (distinct from the grants test above) finds the connected user |
| `test_truncate_table_removes_rows_but_keeps_the_table` | `truncate_table` empties a table without dropping it |
| `test_rename_table_changes_the_visible_name` | `rename_table` updates the name `list_tables` reports |

**Status: all covered, all passing.** Writing `test_list_procedures_finds_a_created_procedure_and_function`,
`test_list_triggers_finds_a_created_trigger`, and `test_list_events_finds_a_created_event`
surfaced a real bug: `execute_query`'s write path always used MySQL's
prepared/binary protocol, which rejects `CREATE PROCEDURE`/`CREATE
FUNCTION`/`CREATE TRIGGER`/`CREATE EVENT` with error 1295 (per MySQL's own
list of statements permitted as prepared statements — these simply aren't on
it). Fixed by routing that stored-program DDL through the text protocol
(`sqlx::raw_sql`), the same way the pre-existing `USE` statement already had
to.

Every provider below now has full CRUD coverage: database/table/index/view
create+drop, insert/update/delete, and a test for that database's specific
upsert idiom (they're all different -- see each section).

## PostgreSQL (`crates/db_client/src/postgres.rs`)

| Test | Proves |
|---|---|
| `test_null_cells_decode_as_none` | Postgres's typed driver decodes NULL correctly (same hypothesis as MySQL, verified separately since the two drivers differ) |
| `test_ping` | Connection + auth work |
| `test_list_databases_finds_public_schema` | Schema listing (Postgres's "database" concept in `db_client` is really a schema within one physical database) |
| `test_create_alter_and_drop_table` | `CREATE SCHEMA`/`CREATE TABLE`/`ALTER TABLE ADD COLUMN`/`DROP TABLE` round trip, verified via `describe_table` before and after |
| `test_create_and_drop_index` | `CREATE UNIQUE INDEX`/`DROP INDEX`, verified via `list_indexes` |
| `test_create_query_and_drop_a_view` | `CREATE VIEW`/`DROP VIEW`, queried directly (Postgres's `list_views` isn't implemented yet, so this doesn't assert through it) |
| `test_insert_update_and_delete_row_lifecycle` | Full row lifecycle: INSERT -> SELECT -> UPDATE -> SELECT -> DELETE -> SELECT-empty |
| `test_upsert_via_on_conflict_do_update` | `INSERT ... ON CONFLICT (key) DO UPDATE` takes the update branch on the second call, not a duplicate insert |
| `test_list_foreign_keys_finds_a_declared_fk` | FK introspection via `information_schema` |
| `test_list_check_constraints_finds_a_declared_check` | CHECK constraint introspection -- asserts the specific named constraint is present rather than a raw count, since Postgres's `information_schema.check_constraints` also reports an implicit not-null-derived entry per NOT NULL column |
| `test_list_procedures_finds_a_created_procedure_and_function` | `pg_proc`-based listing, correctly classified by `ProcedureKind` (Postgres's `prokind` column) |
| `test_list_triggers_finds_a_created_trigger` | Trigger introspection, including the trigger function it calls |
| `test_list_sequences_finds_a_created_sequence` | Sequence introspection (current value + increment) |
| `test_list_users_finds_the_connected_root_user` | `pg_user`-based listing |
| `test_truncate_table_removes_rows_but_keeps_the_table` | `truncate_table` empties a table without dropping it |
| `test_rename_table_changes_the_visible_name` | `rename_table` updates the name `list_tables` reports |
| `test_drop_table_via_provider_method_removes_it` | `drop_table` (the provider method itself, not raw DDL through `execute_query`) removes the table |

**Status: all covered, all passing.** Every test creates and drops its own
scratch schema via `with_scratch_schema`.

## Cassandra / ScyllaDB (`crates/db_client/src/cassandra_provider.rs`)

| Test | Proves |
|---|---|
| `test_ping` | Connection works |
| `connect_times_out_instead_of_hanging_on_an_unreachable_host` | `CONNECT_TIMEOUT` actually bounds a black-holed host instead of hanging |
| `test_schema_and_query_round_trip` | Scratch keyspace/table create, partition+clustering key introspection, insert+select round trip, keyspace cleanup |
| `get_table_ddl_reconstructs_composite_keys_clustering_order_and_static_columns` | DDL reconstruction handles composite PKs, `CLUSTERING ORDER BY`, static columns |
| `get_table_ddl_reconstructs_a_secondary_index_as_a_separate_statement` | Secondary indexes emit their own `CREATE INDEX`, not folded into the table DDL |
| `select_star_on_a_large_table_is_capped_at_max_result_rows` | Unbounded `SELECT *` on Cassandra/Scylla is capped the same way MySQL is |
| `test_materialized_view_round_trip` | `CREATE MATERIALIZED VIEW`, base-table writes propagate to it, `DROP MATERIALIZED VIEW` |
| `test_update_upserts_a_row_that_does_not_exist_yet` | CQL's `UPDATE` is a write-path statement -- it creates the row if the partition key doesn't exist, proven directly rather than assumed |
| `test_delete_row` | `DELETE ... WHERE` removes the row |

**Status: all covered, all passing.** Every test creates and drops its own
scratch keyspace, so this suite needs no fixture data.

**Infra note:** `scylladb/scylla:5.4.9` is pinned deliberately — newer
releases default new keyspaces to tablet replication, which the
`SimpleStrategy` these tests use doesn't support (`--enable-tablets false`
does not fix this). Re-verify before bumping the image.

## MongoDB (`crates/db_client/src/mongo_provider.rs`)

| Test | Proves |
|---|---|
| `test_ping` | Connection works |
| `get_table_ddl_reconstructs_created_indexes` | Plain and unique-named index DDL reconstruction round-trips through a scratch database/collection |
| `test_list_databases_and_collections_finds_scratch_data` | Database and collection listing find newly-written data |
| `test_insert_update_and_delete_document_lifecycle` | Full document lifecycle via the shell-statement parser: `insertOne` -> `findOne` -> `updateOne` (`$set`) -> `findOne` -> `deleteOne` -> `findOne`-empty |
| `test_upsert_via_update_options` | `updateOne` with `{ upsert: true }` creates then updates the same document -- driven through the native driver directly, since the shell-statement parser behind `execute_query` doesn't expose the `upsert` option |
| `test_create_and_query_a_view` | `db.runCommand({ create, viewOn, pipeline })` creates a view that reflects a filtered subset of the source collection |
| `test_list_indexes_finds_created_indexes` | The `list_indexes` trait method (distinct from `get_table_ddl`'s own index reconstruction) finds the implicit `_id_` index plus a plain and a unique named index |

**Status: all covered, all passing.** `list_indexes` was previously
unimplemented for MongoDB (the trait default always returns an empty list),
even though it's wired into the UI's connection tree to populate a
collection's Indexes section -- so that section was silently empty for
every Mongo connection. Implemented via the native driver's `listIndexes`.

## Redis (`crates/db_client/src/redis_provider.rs`)

| Test | Proves |
|---|---|
| `test_ping` | Connection works |
| `test_set_get_and_del_string_key_lifecycle` | `SET`/`GET`/`DEL` round trip |
| `test_set_upserts_an_existing_key` | `SET` on an existing key overwrites it -- Redis's only "upsert" path, since there's no separate insert-vs-update command |
| `test_hash_field_crud` | `HSET`/`HGETALL`/`HGET`/`HDEL` -- create, update a single field, delete a single field |
| `test_execute_query_targets_the_selected_database_index` | A key set in `db1` is invisible from `db0` and visible from `db1` -- proves the numeric-database-index routing in `execute_query` actually isolates data |

**Status: all covered, all passing.**

## Aerospike (`crates/db_client/src/aerospike_provider.rs`)

| Test | Proves |
|---|---|
| `test_ping` | Connection works |
| `test_list_databases_finds_the_test_db_namespace` | Namespace discovery finds the compose stack's `test_db` namespace |
| `test_put_get_and_scan_record_lifecycle` | `put_record`/`get_record`/`scan_records` round trip on a scratch set |
| `test_put_record_upserts_an_existing_key` | `put_record`'s own doc comment says it "creates the record if it does not already exist" -- verified directly: a second put with different bins overwrites in place, no duplicate record |

**Gap:** there is no delete-record path exposed through `DbProvider` for
Aerospike yet (no `delete_record` trait method), so there's no delete test
here -- add one alongside that method when it's implemented.

## ClickHouse (`crates/db_client/src/clickhouse.rs`)

| Test | Proves |
|---|---|
| `test_ping` | HTTP connection + auth work |
| `test_create_and_drop_database` | `CREATE DATABASE`/`DROP DATABASE`, verified via `list_databases` |
| `test_create_table_insert_and_select` | `CREATE TABLE ... ENGINE = MergeTree`, `list_tables` finds it, INSERT + SELECT round trip |
| `test_alter_table_add_column` | `ALTER TABLE ADD COLUMN`, verified via `describe_table` |
| `test_create_and_query_a_view` | `CREATE VIEW`/`DROP VIEW`, queried directly |
| `test_update_and_delete_via_mutations` | `ALTER TABLE ... UPDATE`/`... DELETE` with `SETTINGS mutations_sync = 1` apply synchronously, so the next `SELECT` sees the result deterministically instead of racing a background mutation |
| `test_upsert_via_replacing_merge_tree` | ClickHouse has no `INSERT ... ON CONFLICT`; the idiomatic substitute (`ReplacingMergeTree` + `SELECT ... FINAL`) collapses two same-key rows down to the higher-version one |
| `test_list_views_finds_a_created_view` | `list_views`, cross-checked against `list_tables`' own `TableKind::View` classification |
| `test_truncate_table_removes_rows_but_keeps_the_table` | The trait's default SQL-based `truncate_table` impl works unmodified against ClickHouse |
| `test_rename_table_changes_the_visible_name` | `rename_table`, overridden for ClickHouse (see below) |

**Status: all covered, all passing.** Two real gaps surfaced while writing
these: `list_views` was entirely unimplemented for ClickHouse (the trait
default always returns empty), even though ClickHouse views already showed
up correctly inside `list_tables` via `TableKind::View` -- so the UI's
separate "Views" tree section was silently empty. Implemented via
`system.tables`. Separately, the trait's default `rename_table` (`ALTER
TABLE ... RENAME TO ...`) is not valid ClickHouse syntax -- ClickHouse
requires the standalone `RENAME TABLE ... TO ...` statement -- so
`rename_table` is now overridden for ClickHouse specifically.

## Known non-goals

- **SQLite** has no `_TEST_URL` integration suite and isn't in
  `docker-compose.integration.yml` -- it's file-based, so there's no server
  to containerize; its coverage lives entirely in unit tests against a real
  temp-file database already.
- Aerospike has no delete-record test (see the Aerospike section above --
  the capability isn't exposed through `DbProvider` yet).
- A narrow Windows-only WSL-remote-path flow (`crates/recent_projects/src/recent_projects.rs`)
  is unrelated to this stack and not covered here.
