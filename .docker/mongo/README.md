No init fixture needed here yet: db_client's MongoDB integration test
(`crates/db_client/src/mongo_provider.rs`) creates its own scratch database
and collection. Drop `.js`/`.sh` files in this directory to seed data —
the official `mongo` image runs everything here against the admin database
on first start.
