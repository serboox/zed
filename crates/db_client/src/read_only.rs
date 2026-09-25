use anyhow::Result;

/// Why a request was refused for not being a read. A type of its own, so a
/// caller can tell a refusal from a query that failed.
#[derive(Debug)]
pub struct NotARead(pub String);

impl std::fmt::Display for NotARead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NotARead {}

/// Whether `error` is a refusal for not being a read.
pub fn is_refusal(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<NotARead>())
}

macro_rules! refuse {
    ($($message:tt)*) => {
        return Err(NotARead(format!($($message)*)).into())
    };
}

/// The query languages a read-only request can be checked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    MySql,
    Postgres,
    Sqlite,
    ClickHouse,
    Cql,
}

/// Words a read may start with, per language. Anything else is refused.
fn starters(language: Language) -> &'static [&'static str] {
    match language {
        Language::MySql => &["SELECT", "WITH", "SHOW", "DESCRIBE", "DESC", "EXPLAIN"],
        Language::Postgres => &["SELECT", "WITH", "SHOW", "EXPLAIN", "TABLE", "VALUES"],
        Language::Sqlite => &["SELECT", "WITH", "EXPLAIN", "VALUES"],
        Language::ClickHouse => &[
            "SELECT", "WITH", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "EXISTS",
        ],
        Language::Cql => &["SELECT", "DESCRIBE", "DESC"],
    }
}

/// Words that change data, schema, sessions, locks, files or the server, in any
/// of the dialects. One of them anywhere outside a quoted string refuses the
/// request, even where it would be harmless: a false refusal costs a rewrite, a
/// false pass costs data.
const REFUSED_WORDS: &[&str] = &[
    "INSERT",
    "UPDATE",
    "DELETE",
    "REPLACE",
    "MERGE",
    "UPSERT",
    "TRUNCATE",
    "DROP",
    "ALTER",
    "CREATE",
    "RENAME",
    "GRANT",
    "REVOKE",
    "SET",
    "RESET",
    "CALL",
    "DO",
    "EXEC",
    "EXECUTE",
    "PREPARE",
    "DEALLOCATE",
    "LOAD",
    "IMPORT",
    "COPY",
    "INTO",
    "OUTFILE",
    "DUMPFILE",
    "LOCK",
    "UNLOCK",
    "HANDLER",
    "FLUSH",
    "KILL",
    "PURGE",
    "INSTALL",
    "UNINSTALL",
    "OPTIMIZE",
    "REPAIR",
    "ANALYZE",
    "ANALYSE",
    "VACUUM",
    "REINDEX",
    "CLUSTER",
    "COMMENT",
    "SECURITY",
    "LISTEN",
    "NOTIFY",
    "UNLISTEN",
    "BEGIN",
    "START",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
    "XA",
    "SHUTDOWN",
    "RESTART",
    "ATTACH",
    "DETACH",
    "PRAGMA",
    "CHECKPOINT",
    "DISCARD",
    "REFRESH",
    "SYSTEM",
    "SETTINGS",
    "BATCH",
    "APPLY",
    "FOR",
    "USE",
    "NEXTVAL",
    "SETVAL",
    "SET_CONFIG",
    "GET_LOCK",
    "RELEASE_LOCK",
    "RELEASE_ALL_LOCKS",
    "LO_IMPORT",
    "LO_EXPORT",
    "LO_UNLINK",
    "LO_CREATE",
    "LO_PUT",
    "LO_FROM_BYTEA",
    "QUERY_TO_XML",
    "LOAD_EXTENSION",
    "WRITEFILE",
    "READFILE",
    "EDIT",
];

/// ClickHouse table functions and engines that reach outside the server: they
/// send requests or open files, which `readonly=1` does not stop.
const CLICKHOUSE_REFUSED_WORDS: &[&str] = &[
    "URL",
    "URLCLUSTER",
    "S3",
    "S3CLUSTER",
    "GCS",
    "AZUREBLOBSTORAGE",
    "AZUREBLOBSTORAGECLUSTER",
    "HDFS",
    "HDFSCLUSTER",
    "FILE",
    "FILECLUSTER",
    "REMOTE",
    "REMOTESECURE",
    "CLUSTERALLREPLICAS",
    "MYSQL",
    "POSTGRESQL",
    "SQLITE",
    "MONGODB",
    "REDIS",
    "JDBC",
    "ODBC",
    "EXECUTABLE",
    "INPUT",
    "ICEBERG",
    "DELTALAKE",
    "HUDI",
];

/// Function-name prefixes that act on the server rather than read it.
const REFUSED_PREFIXES: &[&str] = &[
    "PG_TERMINATE",
    "PG_CANCEL",
    "PG_RELOAD",
    "PG_ROTATE",
    "PG_ADVISORY",
    "PG_TRY_ADVISORY",
    "PG_CREATE",
    "PG_DROP",
    "PG_REPLICATION",
    "PG_PROMOTE",
    "PG_SWITCH",
    "PG_START",
    "PG_STOP",
    "PG_LOGICAL",
    "PG_FILE",
    "PG_WRITE",
    "PG_IMPORT",
    "PG_NOTIFY",
    "DBLINK",
];

/// Refuses `text` unless it is provably a single statement that only reads.
///
/// This is the first of two guards: the providers also run whatever passes
/// here where the database itself refuses writes. The check is deliberately
/// narrower than any dialect's grammar, so that the text it sees and the text
/// the server parses cannot disagree about what is a string and what is code:
/// comments of every kind, backslashes and `$` outside a string are refused
/// outright, since each is somewhere a dialect hides code that a simpler
/// reader takes for a string (`/*! DROP */` in MySQL, `$$'$$; DROP` in
/// PostgreSQL, `\'`, `#` comments).
pub fn check_sql(text: &str, language: Language) -> Result<()> {
    let statement = text.trim();
    let statement = statement
        .strip_suffix(';')
        .map(str::trim_end)
        .unwrap_or(statement);
    if statement.is_empty() {
        refuse!("there is no statement to run");
    }
    if statement.contains('\\') {
        refuse!("backslashes are refused in a read-only query");
    }

    let backtick_is_a_quote = matches!(
        language,
        Language::MySql | Language::Sqlite | Language::ClickHouse
    );
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut characters = statement.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\'' | '"' => skip_quoted(character, &mut characters)?,
            '`' if backtick_is_a_quote => skip_quoted('`', &mut characters)?,
            '`' => refuse!("backticks are refused in a read-only query for this database"),
            ';' => refuse!("only one statement can be run at a time"),
            '#' => refuse!("comments are refused in a read-only query"),
            '$' => refuse!("`$` is refused outside a string in a read-only query"),
            '-' if characters.peek() == Some(&'-') => {
                refuse!("comments are refused in a read-only query")
            }
            '/' if matches!(characters.peek(), Some('*') | Some('/')) => {
                refuse!("comments are refused in a read-only query")
            }
            character if character.is_control() && !matches!(character, '\n' | '\r' | '\t') => {
                refuse!("control characters are refused in a read-only query")
            }
            character if character.is_alphanumeric() || character == '_' => {
                word.push(character);
                continue;
            }
            _ => {}
        }
        end_word(&mut word, &mut words);
    }
    end_word(&mut word, &mut words);

    let Some(first) = words.first() else {
        refuse!("there is no statement to run");
    };
    if !starters(language).contains(&first.as_str()) {
        refuse!(
            "only reads can be run here, and `{first}` does not start one (allowed: {})",
            starters(language).join(", ")
        );
    }
    if let Some(refused) = words.iter().find(|word| {
        REFUSED_WORDS.contains(&word.as_str())
            || (language == Language::ClickHouse
                && CLICKHOUSE_REFUSED_WORDS.contains(&word.as_str()))
            || REFUSED_PREFIXES
                .iter()
                .any(|prefix| word.starts_with(prefix))
    }) {
        refuse!("`{refused}` can change data or the server, so it is refused in a read-only query");
    }
    Ok(())
}

fn end_word(word: &mut String, words: &mut Vec<String>) {
    if !word.is_empty() {
        words.push(std::mem::take(word).to_ascii_uppercase());
    }
}

/// Moves past a quoted string or identifier opened by `quote`, where the only
/// escape is the quote doubled -- backslashes were refused before this runs.
fn skip_quoted(
    quote: char,
    characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<()> {
    while let Some(character) = characters.next() {
        if character == quote {
            if characters.peek() == Some(&quote) {
                characters.next();
                continue;
            }
            return Ok(());
        }
    }
    refuse!("a quote is left open")
}

/// Redis commands that read and do nothing else: no writes, no blocking, no
/// scripts, no server or connection state.
const REDIS_READS: &[&str] = &[
    "GET",
    "MGET",
    "GETRANGE",
    "STRLEN",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "HGET",
    "HMGET",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HEXISTS",
    "HSTRLEN",
    "HRANDFIELD",
    "HSCAN",
    "LRANGE",
    "LLEN",
    "LINDEX",
    "LPOS",
    "SMEMBERS",
    "SCARD",
    "SISMEMBER",
    "SMISMEMBER",
    "SRANDMEMBER",
    "SUNION",
    "SINTER",
    "SDIFF",
    "SINTERCARD",
    "SSCAN",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZREVRANGEBYLEX",
    "ZRANK",
    "ZREVRANK",
    "ZSCORE",
    "ZMSCORE",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZRANDMEMBER",
    "ZUNION",
    "ZINTER",
    "ZDIFF",
    "ZSCAN",
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "SCAN",
    "KEYS",
    "DBSIZE",
    "PING",
    "ECHO",
    "INFO",
    "TIME",
    "BITCOUNT",
    "BITPOS",
    "GETBIT",
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEOSEARCH",
    "GEORADIUS_RO",
    "GEORADIUSBYMEMBER_RO",
    "SORT_RO",
    "LCS",
    "BITFIELD_RO",
    "SUBSTR",
    "XINFO",
    "XPENDING",
    "JSON.GET",
    "JSON.MGET",
    "JSON.TYPE",
    "JSON.STRLEN",
    "JSON.ARRLEN",
    "JSON.OBJKEYS",
    "JSON.OBJLEN",
];

/// Commands that read only for some subcommands, and the subcommands that do.
const REDIS_READ_SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "OBJECT",
        &["ENCODING", "FREQ", "IDLETIME", "REFCOUNT", "HELP"],
    ),
    ("MEMORY", &["USAGE", "STATS", "DOCTOR", "HELP"]),
];

/// Refuses a Redis command unless it only reads. The provider sends the whole
/// text as one command, so the command word decides what it does.
pub fn check_redis(text: &str) -> Result<()> {
    let mut tokens = text.split_whitespace();
    let Some(command) = tokens.next().map(str::to_ascii_uppercase) else {
        refuse!("there is no command to run");
    };
    if REDIS_READS.contains(&command.as_str()) {
        return Ok(());
    }
    if let Some((_, allowed)) = REDIS_READ_SUBCOMMANDS
        .iter()
        .find(|(name, _)| *name == command)
    {
        let subcommand = tokens
            .next()
            .map(str::to_ascii_uppercase)
            .unwrap_or_default();
        if allowed.contains(&subcommand.as_str()) {
            return Ok(());
        }
    }
    refuse!("`{command}` is not a Redis command that only reads, so it is refused")
}

/// Refuses an aggregation pipeline that writes its result somewhere: `$out`
/// and `$merge` anywhere in it, nested stages included.
pub fn check_mongo_pipeline(pipeline: &[mongodb::bson::Document]) -> Result<()> {
    fn writes(value: &mongodb::bson::Bson) -> bool {
        match value {
            mongodb::bson::Bson::Document(document) => document
                .iter()
                .any(|(key, value)| key == "$out" || key == "$merge" || writes(value)),
            mongodb::bson::Bson::Array(values) => values.iter().any(writes),
            _ => false,
        }
    }
    if pipeline
        .iter()
        .any(|stage| writes(&mongodb::bson::Bson::Document(stage.clone())))
    {
        refuse!("`$out` and `$merge` write the result into a collection, so they are refused");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads(text: &str, language: Language) -> bool {
        check_sql(text, language).is_ok()
    }

    const ALL: [Language; 4] = [
        Language::MySql,
        Language::Postgres,
        Language::Sqlite,
        Language::ClickHouse,
    ];

    #[test]
    fn plain_reads_pass() {
        for language in ALL {
            assert!(reads("SELECT id, name FROM users WHERE id = 1", language));
            assert!(
                reads("  select * from t ;  ", language),
                "a trailing ; is fine"
            );
            assert!(reads("WITH x AS (SELECT 1 AS a) SELECT a FROM x", language));
            assert!(reads("SELECT 'DROP TABLE t; --' AS note", language));
            assert!(reads("SELECT 'it''s' AS quoted", language));
        }
        assert!(reads("SHOW TABLES", Language::MySql));
        assert!(reads("DESCRIBE users", Language::MySql));
        assert!(reads(
            "SELECT JSON_EXTRACT(doc, '$.name') FROM t",
            Language::MySql
        ));
        assert!(reads("EXPLAIN SELECT * FROM t", Language::Postgres));
        assert!(reads(
            "SELECT \"order\" FROM \"Orders\"",
            Language::Postgres
        ));
        assert!(reads("SELECT `order` FROM `orders`", Language::MySql));
        assert!(reads("SELECT * FROM users WHERE id = 1", Language::Cql));
    }

    #[test]
    fn every_kind_of_write_is_refused() {
        for language in ALL {
            for text in [
                "INSERT INTO t VALUES (1)",
                "UPDATE t SET a = 1",
                "DELETE FROM t",
                "REPLACE INTO t VALUES (1)",
                "TRUNCATE t",
                "DROP TABLE t",
                "ALTER TABLE t ADD c INT",
                "CREATE TABLE t (a INT)",
                "GRANT ALL ON t TO u",
                "CALL p()",
                "SET a = 1",
                "BEGIN",
                "COMMIT",
                "LOCK TABLES t WRITE",
                "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
                "SELECT * INTO new_table FROM t",
                "SELECT * FROM t INTO OUTFILE 'x'",
                "SELECT * FROM t FOR UPDATE",
                "EXPLAIN ANALYZE DELETE FROM t",
                "EXPLAIN ANALYZE SELECT * FROM t",
                "SELECT nextval('s')",
                "SELECT pg_terminate_backend(1)",
                "SELECT GET_LOCK('x', 1)",
                "SELECT * FROM dblink('x', 'DELETE FROM t')",
                "USE other",
                "VACUUM",
                "PRAGMA writable_schema = 1",
                "SELECT load_extension('/tmp/x.so')",
                "SELECT writefile('/tmp/x', 'data')",
                "EXPLAIN (ANALYZE) SELECT 1",
                "SELECT pg_notify('c', 'x')",
                "SELECT dblink_exec('x', 'DELETE FROM t')",
            ] {
                assert!(!reads(text, language), "{language:?} let through: {text}");
            }
        }
    }

    #[test]
    fn writes_hidden_where_a_dialect_reads_code_are_refused() {
        for language in ALL {
            for text in [
                // A second statement after a read.
                "SELECT 1; DROP TABLE t",
                "SELECT 1;DELETE FROM t;",
                // MySQL runs the inside of `/*! ... */`.
                "SELECT 1 /*! ; DROP TABLE t */",
                "SELECT /*+ hint */ 1",
                // A comment that one reader ends where another does not.
                "SELECT 1 # '\n; DROP TABLE t; -- '",
                "SELECT 1 -- '\nDELETE FROM t",
                // PostgreSQL dollar quoting holds a quote the simple reader
                // would take for the start of a string.
                "SELECT $$'$$; DROP TABLE t; --'",
                "SELECT $tag$ x $tag$",
                // Backslash escapes mean different things per dialect.
                "SELECT 'a\\'; DROP TABLE t; --'",
                "SELECT E'\\x41'",
                // An open quote hides whatever follows it.
                "SELECT 'never closed",
            ] {
                assert!(!reads(text, language), "{language:?} let through: {text}");
            }
        }
        assert!(
            !reads("SELECT `a` FROM t", Language::Postgres),
            "a backtick is not a quote to PostgreSQL"
        );
        assert!(!reads("SELECT * FROM t // x", Language::Cql));
    }

    #[test]
    fn only_a_read_may_start_the_statement() {
        assert!(!reads("SHOW TABLES", Language::Sqlite));
        assert!(!reads("OPTIMIZE TABLE t", Language::ClickHouse));
        assert!(!reads("SYSTEM FLUSH LOGS", Language::ClickHouse));
        for reaches_out in [
            "SELECT * FROM url('https://example.com/x', 'JSONEachRow')",
            "SELECT * FROM s3('https://bucket/x.csv')",
            "SELECT * FROM remote('host', db.t)",
            "SELECT * FROM file('x.csv')",
            "SELECT * FROM mysql('host:3306', 'db', 't', 'u', 'p')",
        ] {
            assert!(!reads(reaches_out, Language::ClickHouse), "{reaches_out}");
        }
        assert!(
            reads("SELECT url FROM `pages`", Language::MySql),
            "only ClickHouse refuses these"
        );
        assert!(!reads("INSERT INTO t (a) VALUES (1)", Language::Cql));
        assert!(!reads(
            "BEGIN BATCH DELETE FROM t WHERE a = 1 APPLY BATCH",
            Language::Cql
        ));
        assert!(!reads("", Language::MySql));
        assert!(!reads(" ; ", Language::MySql));
    }

    #[test]
    fn redis_reads_pass_and_everything_else_is_refused() {
        for text in [
            "GET key",
            "hgetall user:1",
            "SCAN 0 MATCH a*",
            "OBJECT ENCODING k",
            "MEMORY USAGE k",
            "BITFIELD_RO k GET u8 0",
            "ZRANGEBYSCORE z 0 10",
            "XRANGE s - +",
            "SMEMBERS set",
            "LRANGE list 0 -1",
        ] {
            assert!(check_redis(text).is_ok(), "{text}");
        }
        for text in [
            "SET k v",
            "DEL k",
            "GETDEL k",
            "GETEX k PX 1",
            "GETSET k v",
            "EVAL \"return 1\" 0",
            "FCALL f 0",
            "SORT k STORE d",
            "FLUSHALL",
            "CONFIG SET x y",
            "CLIENT KILL x",
            "SUBSCRIBE c",
            "MONITOR",
            "OBJECT",
            "MEMORY PURGE",
            "SELECT 1",
            "",
        ] {
            assert!(check_redis(text).is_err(), "let through: {text}");
        }
    }

    #[test]
    fn a_pipeline_that_writes_its_result_is_refused() {
        use mongodb::bson::doc;
        assert!(check_mongo_pipeline(&[doc! { "$match": { "a": 1 } }]).is_ok());
        assert!(check_mongo_pipeline(&[doc! { "$match": {} }, doc! { "$out": "copy" }]).is_err());
        assert!(check_mongo_pipeline(&[doc! { "$merge": { "into": "copy" } }]).is_err());
        assert!(
            check_mongo_pipeline(&[doc! {
                "$facet": { "a": [ { "$merge": { "into": "copy" } } ] }
            }])
            .is_err(),
            "nested stages are looked into as well"
        );
    }
}
