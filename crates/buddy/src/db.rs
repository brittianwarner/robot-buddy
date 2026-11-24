//! Actor `SQLite` access, row readers, text binders and the bot key every actor is
//! addressed by.
//!
//! Every statement and transaction goes through [`Db::db`], which counts them so a test can
//! measure what one turn costs instead of reading it from the code.
use anyhow::{Result, bail};
use rivetkit::{
    Actor, ActorKeySegment, BindParam, ColumnValue, Ctx, QueryResult, SqliteDb, SqliteDbExt,
    SqliteTransaction, SqliteTransactionOptions,
};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// How long an ordinary transaction may hold the write gate once it is active. Every
/// transaction here is a few local statements; none waits on the network.
pub const TX_TIMEOUT: Duration = Duration::from_secs(10);
/// For batch work that touches many rows: history rebuild and retention batches.
pub const BATCH_TX_TIMEOUT: Duration = Duration::from_secs(30);

/// Transaction options with a static name, which Rivet reports in errors and profiles,
/// and the ordinary timeout.
pub const fn named(name: &'static str) -> SqliteTransactionOptions<'static> {
    named_for(name, TX_TIMEOUT)
}

/// Transaction options with a static name and an explicit timeout.
pub const fn named_for(name: &'static str, timeout: Duration) -> SqliteTransactionOptions<'static> {
    SqliteTransactionOptions {
        name: Some(name),
        timeout: Some(timeout),
    }
}

static STATEMENTS: AtomicU64 = AtomicU64::new(0);
static AUTOCOMMIT: AtomicU64 = AtomicU64::new(0);
static TRANSACTIONS: AtomicU64 = AtomicU64::new(0);

/// Statements and transactions issued through [`Db::db`] by this process since it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SqlCounts {
    /// Every statement, inside a transaction or not.
    pub statements: u64,
    /// Statements that ran outside a transaction, each its own commit.
    pub autocommit: u64,
    /// Named transactions begun.
    pub transactions: u64,
}

impl SqlCounts {
    /// What ran between `earlier` and `self`.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Self {
        Self {
            statements: self.statements.saturating_sub(earlier.statements),
            autocommit: self.autocommit.saturating_sub(earlier.autocommit),
            transactions: self.transactions.saturating_sub(earlier.transactions),
        }
    }
}

/// The process-wide counts. Two relaxed atomics per statement; read by tests.
pub fn sql_counts() -> SqlCounts {
    SqlCounts {
        statements: STATEMENTS.load(Ordering::Relaxed),
        autocommit: AUTOCOMMIT.load(Ordering::Relaxed),
        transactions: TRANSACTIONS.load(Ordering::Relaxed),
    }
}

fn count_statement(autocommit: bool) {
    STATEMENTS.fetch_add(1, Ordering::Relaxed);
    if autocommit {
        AUTOCOMMIT.fetch_add(1, Ordering::Relaxed);
    }
}

/// The result of a statement that may change rows.
pub struct Executed {
    pub rows: Vec<Vec<ColumnValue>>,
    pub changes: i64,
    pub last_insert_row_id: Option<i64>,
}

/// Counted access to an actor's database.
pub struct Sql<'a>(&'a SqliteDb);

/// Counted database access for an actor context.
pub trait Db {
    /// The actor's database.
    fn db(&self) -> Sql<'_>;
}

impl<A: Actor> Db for Ctx<A> {
    fn db(&self) -> Sql<'_> {
        Sql(self.sql())
    }
}

impl Sql<'_> {
    /// One read outside a transaction.
    pub async fn query(&self, sql: &str, params: Option<Vec<BindParam>>) -> Result<QueryResult> {
        count_statement(true);
        self.0.query(sql, params).await
    }

    /// One write outside a transaction: its own commit.
    pub async fn execute(&self, sql: &str, params: Option<Vec<BindParam>>) -> Result<Executed> {
        count_statement(true);
        let result = self.0.execute(sql, params).await?;
        Ok(Executed {
            rows: result.rows,
            changes: result.changes,
            last_insert_row_id: result.last_insert_row_id,
        })
    }

    /// A batch of statements without bindings, such as the schema.
    pub async fn exec(&self, sql: &str) -> Result<QueryResult> {
        count_statement(true);
        self.0.exec(sql).await
    }

    /// Runs `callback` in one named transaction: commit on `Ok`, rollback on `Err`. Use
    /// `tx` only inside it, and make no network or actor call there.
    pub async fn transaction<T, F, Fut>(
        &self,
        callback: F,
        options: SqliteTransactionOptions<'static>,
    ) -> Result<T>
    where
        T: Send,
        F: FnOnce(Tx) -> Fut + Send,
        Fut: Future<Output = Result<T>> + Send,
    {
        TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
        self.0
            .transaction(move |tx| callback(Tx(tx)), options)
            .await
    }
}

/// A counted statement runner inside one transaction.
pub struct Tx(SqliteTransaction);

impl Tx {
    pub async fn execute(&self, sql: &str, params: Option<Vec<BindParam>>) -> Result<Executed> {
        count_statement(false);
        let result = self.0.execute(sql, params).await?;
        Ok(Executed {
            rows: result.rows,
            changes: result.changes,
            last_insert_row_id: result.last_insert_row_id,
        })
    }

    pub async fn exec(&self, sql: &str) -> Result<QueryResult> {
        count_statement(false);
        self.0.exec(sql).await
    }
}

/// Reads a text column.
pub fn string(row: &[ColumnValue], index: usize) -> Result<String> {
    match row.get(index) {
        Some(ColumnValue::Text(value)) => Ok(value.clone()),
        _ => bail!("invalid stored text"),
    }
}

/// Reads an integer column.
pub fn integer(row: &[ColumnValue], index: usize) -> Result<i64> {
    match row.get(index) {
        Some(ColumnValue::Integer(value)) => Ok(*value),
        _ => bail!("invalid stored integer"),
    }
}

/// Binds text values in statement order.
pub fn params(values: &[&str]) -> Option<Vec<BindParam>> {
    Some(
        values
            .iter()
            .map(|value| BindParam::Text((*value).to_owned()))
            .collect(),
    )
}

/// The bot ID: the actor key, which is exactly one string segment. Any other key, such
/// as `[bot, extra]` or a number, is refused, so two key shapes never share a bot's grants.
pub fn bot_key<A: Actor>(ctx: &Ctx<A>) -> Result<String> {
    key_bot(ctx.key()).map(str::to_owned)
}

fn key_bot(key: &[ActorKeySegment]) -> Result<&str> {
    match key {
        [ActorKeySegment::String(bot)] if !bot.is_empty() => Ok(bot),
        _ => bail!("bot requires a single string key"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_key_is_exactly_one_string_segment() {
        let text = |value: &str| ActorKeySegment::String(value.to_owned());
        assert_eq!(key_bot(&[text("bot-1")]).unwrap(), "bot-1");
        assert!(key_bot(&[]).is_err());
        assert!(key_bot(&[text("")]).is_err());
        assert!(key_bot(&[text("bot-1"), text("extra")]).is_err());
        assert!(key_bot(&[ActorKeySegment::Number(1.0)]).is_err());
        assert!(key_bot(&[ActorKeySegment::Number(1.0), text("bot-1")]).is_err());
    }

    #[test]
    fn readers_check_column_types() {
        let row = [
            ColumnValue::Text("turn".into()),
            ColumnValue::Integer(7),
            ColumnValue::Null,
        ];
        assert_eq!(string(&row, 0).unwrap(), "turn");
        assert_eq!(integer(&row, 1).unwrap(), 7);
        assert!(string(&row, 1).is_err());
        assert!(integer(&row, 0).is_err());
        assert!(string(&row, 2).is_err());
        assert!(integer(&row, 3).is_err());
    }

    #[test]
    fn params_bind_text_in_order() {
        let bound = params(&["a", "b"]).unwrap();
        assert!(matches!(
            bound.as_slice(),
            [BindParam::Text(a), BindParam::Text(b)] if a == "a" && b == "b"
        ));
    }
}
