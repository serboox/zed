No init fixture needed here yet: db_client's ClickHouse integration tests
create and drop their own scratch databases/tables. Drop `.sql` files in this
directory to seed data — the official `clickhouse-server` image runs
everything here against `CLICKHOUSE_DB` on first start.
