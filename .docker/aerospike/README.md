No fixture here: the `aerospike` service is configured entirely through
`docker-compose.yml`'s environment variables (namespace, memory, storage).
db_client's Aerospike integration tests seed and clean up their own scratch
records under the `test_db` namespace.
