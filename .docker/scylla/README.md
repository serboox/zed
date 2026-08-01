No init fixture needed here yet: db_client's Cassandra/ScyllaDB integration
tests (`crates/db_client/src/cassandra_provider.rs`) create their own scratch
keyspaces and drop them when done. This directory is mounted at
`/docker-entrypoint-initdb.d`, but the official `scylladb/scylla` image does
not currently run scripts from it — kept for parity with the other services
and in case a future image version (or a wrapper entrypoint) adds support.
