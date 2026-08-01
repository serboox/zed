No init fixture needed here yet: db_client's Postgres integration tests
(`crates/db_client/src/postgres.rs`) only touch the default `public` schema
and scratch tables they create and drop themselves. Drop `.sql`/`.sql.gz`
files in this directory if a future test needs pre-seeded data — Postgres's
official image runs everything here against `POSTGRES_DB` on first start.
