use anyhow::Result;
use async_trait::async_trait;

use crate::schema::{
    ColumnInfo, DatabaseInfo, FkInfo, IndexInfo, ProcedureInfo, QueryResult, TableInfo,
    TriggerInfo, UserInfo,
};

#[async_trait]
pub trait DbProvider: Send + Sync {
    async fn ping(&self) -> Result<()>;
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>>;
    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>>;
    async fn describe_table(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>>;
    async fn execute_query(&self, database: &str, sql: &str) -> Result<QueryResult>;
    async fn get_table_ddl(&self, database: &str, table: &str) -> Result<String>;

    async fn list_views(&self, _database: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn list_indexes(&self, _database: &str, _table: &str) -> Result<Vec<IndexInfo>> {
        Ok(Vec::new())
    }

    async fn list_foreign_keys(&self, _database: &str, _table: &str) -> Result<Vec<FkInfo>> {
        Ok(Vec::new())
    }

    async fn list_procedures(&self, _database: &str) -> Result<Vec<ProcedureInfo>> {
        Ok(Vec::new())
    }

    async fn list_triggers(&self, _database: &str, _table: &str) -> Result<Vec<TriggerInfo>> {
        Ok(Vec::new())
    }

    async fn list_users(&self) -> Result<Vec<UserInfo>> {
        Ok(Vec::new())
    }

    async fn truncate_table(&self, database: &str, table: &str) -> Result<()> {
        self.execute_query(database, &format!("TRUNCATE TABLE {}", table)).await?;
        Ok(())
    }

    async fn drop_table(&self, database: &str, table: &str) -> Result<()> {
        self.execute_query(database, &format!("DROP TABLE {}", table)).await?;
        Ok(())
    }

    async fn rename_table(&self, database: &str, old_name: &str, new_name: &str) -> Result<()> {
        self.execute_query(database, &format!("ALTER TABLE {} RENAME TO {}", old_name, new_name)).await?;
        Ok(())
    }
}
