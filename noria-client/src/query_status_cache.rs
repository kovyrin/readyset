//! This implements the SeenCache component of [Readyset Query Handling][doc]
//!
//! [doc]: https://docs.google.com/document/d/1GUwLwklpwVlX0fuXSUspn_uFLC2jNEEHo4WUJX8yHbg/edit
//!
//! The QueryStatusCache maintains the QueryStatus for each parsed query
//! seen in the adapter. The QueryStatus assigned to each query influences
//! how the query is handled in the adapter.
//!
//! If the query:
//!   - NeedsProcessing: The query should be sent to the fallback database.
//!   - Allowed: The query should be sent to noria.
//!   - Is not in the cache: The queries status should be determined and
//!                          set to either NeedsProcesing or Allowed.

use chrono::{DateTime, Utc};
use nom_sql::SelectStatement;
use std::collections::HashMap;

/// Holds metadata regarding when a query was first seen within the system,
/// along with its current state.
#[derive(Debug, Clone)]
struct QueryStatus {
    first_seen: DateTime<Utc>,
    state: QueryState,
}

impl QueryStatus {
    fn new(state: QueryState) -> Self {
        Self {
            first_seen: Utc::now(),
            state,
        }
    }
}

/// Each query is uniquely identifier by its select statement
type Query = SelectStatement;

/// Represents the current state of any given query. Deny is an implicit state
/// that is derived from a combination of NeedsProcessing and other query
/// status metadata.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryState {
    NeedsProcessing,
    Allow,
}

/// Represents all queries that have been seen in the system, along with
/// metadata about when the query was first seen, and what state it's currently
/// in. QueryStatusCache is thread safe. It is intended that only one
/// QueryStatusCache is spun up per adapter.
pub struct QueryStatusCache {
    /// A thread-safe hash map that holds the query status of each query
    /// that is cached.
    inner: tokio::sync::RwLock<HashMap<Query, QueryStatus>>,
    /// Defines a maximum age that any query may stay in the QueryStatusCache
    /// with a state of QueryState::NeedsProcessing before it is inferred to be
    /// denied. If a query is denied it is sent exclusively to fallback.
    max_processing: chrono::Duration,
}

impl QueryStatusCache {
    /// Construct a new QueryStatusCache. Requires a duration for max processing
    /// time, which will be used to infer the deny list, as well as to cease
    /// processing queries past a given age.
    pub fn new(max_processing: chrono::Duration) -> QueryStatusCache {
        QueryStatusCache {
            inner: tokio::sync::RwLock::new(HashMap::new()),
            max_processing,
        }
    }

    /// Helper method to assist in computing a DateTime<Utc> of the oldest age we will
    /// currently process for any given query in the cache.
    fn max_age(&self) -> chrono::DateTime<Utc> {
        Utc::now() - self.max_processing
    }

    /// Registers a query with a default state of [`QueryState::NeedsProcessing`] if
    /// it doesn't already exist in the cache. If it does exist in the cache this
    /// will no-op. Can only be called by proxy of calling `exists` which will check
    /// if we have seen a query or not, and if not register it.
    pub async fn register_query(&self, query: &Query) {
        self.inner
            .write()
            .await
            .entry(query.clone())
            .or_insert_with(|| QueryStatus::new(QueryState::NeedsProcessing));
    }

    /// Sets the provided query to have a QueryState of Allow.
    /// If not found we no-op. This function should never be called with a query that
    /// isn't already registered in the cache.
    pub async fn set_allow(&self, query: &Query) {
        self.inner.write().await.get_mut(query).map(|s| {
            s.state = QueryState::Allow;
            s
        });
    }

    /// Sets the provided query to have a QueryState of NeedsProcessing.
    /// If the query is not found this is a no-op.
    pub async fn set_needs_processing(&self, query: &Query) {
        self.inner.write().await.get_mut(query).map(|s| {
            s.state = QueryState::NeedsProcessing;
            s
        });
    }

    /// Returns a list of queries that currently need the be processed to determine
    /// if they should be allowed (are supported by Noria).
    pub async fn needs_processing(&self) -> Vec<Query> {
        self.inner
            .read()
            .await
            .iter()
            .filter(|(_, status)| matches!(status.state, QueryState::NeedsProcessing if status.first_seen >= self.max_age()))
            .map(|(q, _)| q.clone())
            .collect()
    }

    /// Returns a list of queries that have a state of [`QueryState::Allow`].
    pub async fn allow_list(&self) -> Vec<Query> {
        self.inner
            .read()
            .await
            .iter()
            .filter(|(_, status)| matches!(status.state, QueryState::Allow))
            .map(|(q, _)| q.clone())
            .collect()
    }

    /// Returns a list of queries that are in the deny list.
    pub async fn deny_list(&self) -> Vec<Query> {
        self.inner
            .read()
            .await
            .iter()
            .filter(|(_, status)| matches!(status.state, QueryState::NeedsProcessing if status.first_seen < self.max_age())
            )
            .map(|(q, _)| q.clone())
            .collect()
    }

    /// Returns the a query's current status in the cache. If the query does not
    /// exist this returns None.
    pub async fn query_state(&self, query: &Query) -> Option<QueryState> {
        self.inner.read().await.get(query).map(|s| s.state.clone())
    }

    /// Returns whether the given query exists in the query status cache.
    pub async fn exists(&self, query: &Query) -> bool {
        self.inner.read().await.contains_key(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use nom_sql::SqlQuery;

    fn test_cache() -> QueryStatusCache {
        QueryStatusCache::new(Duration::minutes(1))
    }

    fn select_statement(s: &str) -> anyhow::Result<SelectStatement> {
        match nom_sql::parse_query(nom_sql::Dialect::MySQL, s) {
            Ok(SqlQuery::Select(s)) => Ok(s),
            _ => Err(anyhow::anyhow!("Invalid SELECT statement")),
        }
    }

    #[tokio::test]
    async fn query_is_allowed() {
        let cache = test_cache();
        let query = select_statement("SELECT * FROM t1").unwrap();
        assert_eq!(cache.needs_processing().await.len(), 0);
        assert_eq!(cache.allow_list().await.len(), 0);
        assert_eq!(cache.deny_list().await.len(), 0);
        assert_eq!(cache.query_state(&query).await, None);

        cache.register_query(&query).await;
        assert_eq!(cache.needs_processing().await, vec![query.clone()]);
        assert_eq!(cache.allow_list().await.len(), 0);
        assert_eq!(cache.deny_list().await.len(), 0);
        assert_eq!(
            cache.query_state(&query).await,
            Some(QueryState::NeedsProcessing)
        );

        cache.set_allow(&query).await;
        assert_eq!(cache.needs_processing().await.len(), 0);
        assert_eq!(cache.allow_list().await, vec![query.clone()]);
        assert_eq!(cache.deny_list().await.len(), 0);
        assert_eq!(cache.query_state(&query).await, Some(QueryState::Allow));
    }

    #[tokio::test]
    async fn repeated_prepares() {
        let cache = test_cache();
        let query = select_statement("SELECT * FROM t1").unwrap();

        cache.register_query(&query).await;
        cache.register_query(&query).await;
        assert_eq!(cache.needs_processing().await.len(), 1);
    }
}