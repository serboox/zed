No init fixture here: Redis has no equivalent of `docker-entrypoint-initdb.d`.
db_client's Redis integration tests seed and clean up their own scratch keys.
