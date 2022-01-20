//! The query status cache provides a thread-safe window into an adapter's
//! knowledge about queries, currently the migration status of a query in
//! Noria.
use std::time::{Duration, Instant};

use crate::rewrite::anonymize_literals;
use dashmap::DashMap;
use nom_sql::SelectStatement;
use serde::{ser::SerializeSeq, Serialize, Serializer};

/// Each query is uniquely identifier by its select statement
type Query = SelectStatement;

#[derive(Debug, Clone, PartialEq)]
pub struct QueryStatus {
    pub migration_state: MigrationState,
    pub execution_info: Option<ExecutionInfo>,
}

impl QueryStatus {
    fn new() -> Self {
        Self {
            migration_state: MigrationState::Pending,
            execution_info: None,
        }
    }
}

/// Represents the current migration state of a given query. This state should be updated any time
/// a migration is performed, or we learn that the migration state has changed, i.e. we receive a
/// ViewNotFound error indicating a query is not migrated.
#[derive(Debug, Clone, PartialEq)]
pub enum MigrationState {
    /// A migration has not been completed for this query. There may be one in progress depending
    /// on the adapters MigrationMode.
    Pending,
    /// This query has been migrated and a view exists.
    Successful,
    /// This query is not supported and should not be tried against Noria.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionInfo {
    pub state: ExecutionState,
    pub last_transition_time: Instant,
}

impl ExecutionInfo {
    /// Used to update the inner state type, if our current state is something different, and
    /// update the last transition time accordingly.
    fn update_inner(&mut self, state: ExecutionState) {
        self.last_transition_time = Instant::now();
        self.state = state;
    }

    /// Update ExecutionInfo to indicate that a recent execute succeeded.
    pub fn execute_succeeded(&mut self) {
        if matches!(self.state, ExecutionState::Successful) {
            return;
        }

        self.update_inner(ExecutionState::Successful)
    }

    /// Update ExecutionInfo to indicate that a recent execute failed due to a networking error.
    pub fn execute_network_failure(&mut self) {
        if matches!(self.state, ExecutionState::NetworkFailure) {
            return;
        }

        self.update_inner(ExecutionState::NetworkFailure)
    }

    /// Update ExecutionInfo to indicate that a recent execute failed due to some reason other than
    /// a networking error.
    pub fn execute_failed(&mut self) {
        if matches!(self.state, ExecutionState::Failed) {
            return;
        }

        self.update_inner(ExecutionState::Failed)
    }

    /// Resets the internal transition time to now. This should be used with extreme caution.
    pub fn reset_transition_time(&mut self) {
        self.last_transition_time = Instant::now();
    }

    /// Resets the transition time for the query if we have exceeded the recovery window.
    /// Returns true if data was mutated and false if not.
    pub fn reset_if_exceeded_recovery(
        &mut self,
        query_max_failure_duration: Duration,
        fallback_recovery_duration: Duration,
    ) -> bool {
        if self.execute_network_failure_exceeded(
            query_max_failure_duration + fallback_recovery_duration,
        ) {
            // We've exceeded the window, so we'll reset the transition time. This should
            // ensure it gets tried again the next time. If it fails again due to a networking
            // error, it will get automatically set to the NetworkFailure state with an updated
            // transition time, which will eventually retrigger the fallback
            // recovery window.
            self.reset_transition_time();
            true
        } else {
            false
        }
    }

    /// If the current ExecutionState is ExecutionState::NetworkFailure, then this method will
    /// return true if that state has persisted for longer than the supplied duration, otherwise,
    /// it will return false.
    pub fn execute_network_failure_exceeded(&self, duration: Duration) -> bool {
        if let ExecutionState::NetworkFailure = self.state {
            return self.last_transition_time.elapsed() > duration;
        }

        false
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionState {
    Successful,
    NetworkFailure,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub struct QueryList {
    queries: Vec<Query>,
}

impl QueryList {
    pub fn len(&self) -> usize {
        self.queries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }
}

impl From<Vec<Query>> for QueryList {
    fn from(queries: Vec<Query>) -> Self {
        QueryList { queries }
    }
}

impl IntoIterator for QueryList {
    type Item = Query;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.queries.into_iter()
    }
}

impl Serialize for QueryList {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sanitized = self.queries.clone();
        sanitized.iter_mut().for_each(anonymize_literals);

        let mut seq = serializer.serialize_seq(Some(sanitized.len()))?;
        for q in sanitized {
            seq.serialize_element(&q.to_string())?;
        }
        seq.end()
    }
}

/// A metadata cache for all queries that have been processed by this
/// adapter. Thread-safe.
pub struct QueryStatusCache {
    /// A thread-safe hash map that holds the query status of each query
    /// that is cached.
    inner: DashMap<Query, QueryStatus>,

    /// Holds the current style of migration, whether async or explicit, which may change the
    /// behavior of some internal methods.
    style: MigrationStyle,
}

impl Default for QueryStatusCache {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryStatusCache {
    /// Constructs a new QueryStatusCache with the migration style set to Async.
    pub fn new() -> QueryStatusCache {
        QueryStatusCache {
            inner: DashMap::new(),
            style: MigrationStyle::InRequestPath,
        }
    }

    pub fn with_style(style: MigrationStyle) -> QueryStatusCache {
        QueryStatusCache {
            inner: DashMap::new(),
            style,
        }
    }

    /// This function returns the query migration state of a query. If the query does not exist
    /// within the query status cache, an entry is created and the query is set to
    /// PendingMigration.
    pub fn query_migration_state(&self, q: &Query) -> MigrationState {
        let query_state = self.inner.get(q).map(|m| m.migration_state.clone());
        match query_state {
            Some(s) => s,
            None => {
                self.inner.insert(q.clone(), QueryStatus::new());
                MigrationState::Pending
            }
        }
    }

    /// This function returns the query status of a query. If the query does not exist
    /// within the query status cache, an entry is created and the query is set to
    /// PendingMigration.
    pub fn query_status(&self, q: &Query) -> QueryStatus {
        match self.inner.get(q).map(|s| s.clone()) {
            Some(s) => s,
            None => {
                self.inner.insert(q.clone(), QueryStatus::new());
                QueryStatus::new()
            }
        }
    }

    /// Updates the execution info for the given query.
    pub fn update_execution_info(&self, q: &Query, info: ExecutionInfo) {
        if let Some(mut s) = self.inner.get_mut(q) {
            s.execution_info = Some(info);
        }
    }

    /// Updates the transition time in the execution info for the given query.
    pub fn update_transition_time(&self, q: &Query, transition: &std::time::Instant) {
        if let Some(mut s) = self.inner.get_mut(q) {
            if let Some(ref mut info) = s.execution_info {
                info.last_transition_time = *transition;
            }
        }
    }

    /// Resets the internal transition time to now. This should be used with extreme caution.
    pub fn reset_transition_time(&self, q: &Query) {
        if let Some(mut s) = self.inner.get_mut(q) {
            if let Some(ref mut info) = s.execution_info {
                info.last_transition_time = Instant::now()
            }
        }
    }

    /// Resets the transition time for the query if we have exceeded the recovery window.
    /// Returns true if data was mutated, and false if it was not.
    pub fn reset_if_exceeded_recovery(
        &self,
        stmt: &nom_sql::SelectStatement,
        query_max_failure_duration: Duration,
        fallback_recovery_duration: Duration,
    ) -> bool {
        if let Some(ref mut s) = self.inner.get_mut(stmt) {
            if let Some(ref mut info) = s.execution_info {
                return info.reset_if_exceeded_recovery(
                    query_max_failure_duration,
                    fallback_recovery_duration,
                );
            }
        }

        false
    }

    /// Update ExecutionInfo to indicate that a recent execute failed due to a networking problem.
    pub fn execute_network_failure(&self, q: &Query) {
        if let Some(mut s) = self.inner.get_mut(q) {
            match s.execution_info {
                Some(ref mut info) => info.execute_network_failure(),
                None => {
                    s.execution_info = Some(ExecutionInfo {
                        state: ExecutionState::NetworkFailure,
                        last_transition_time: Instant::now(),
                    });
                }
            }
        }
    }

    /// Update ExecutionInfo to indicate that a recent execute succeeded.
    pub fn execute_succeeded(&self, q: &Query) {
        if let Some(mut s) = self.inner.get_mut(q) {
            match s.execution_info {
                Some(ref mut info) => info.execute_succeeded(),
                None => {
                    s.execution_info = Some(ExecutionInfo {
                        state: ExecutionState::Successful,
                        last_transition_time: Instant::now(),
                    });
                }
            }
        }
    }

    /// Update ExecutionInfo to indicate that a recent execute failed.
    pub fn execute_failed(&self, q: &Query) {
        if let Some(mut s) = self.inner.get_mut(q) {
            match s.execution_info {
                Some(ref mut info) => info.execute_failed(),
                None => {
                    s.execution_info = Some(ExecutionInfo {
                        state: ExecutionState::Failed,
                        last_transition_time: Instant::now(),
                    });
                }
            }
        }
    }

    /// If the current ExecutionState is ExecutionState::NetworkFailure, then this method will
    /// return true if that state has persisted for longer than the supplied duration, otherwise,
    /// it will return false.
    pub fn execute_network_failure_exceeded(&self, q: &Query, duration: Duration) -> bool {
        if let Some(ref s) = self.inner.get(q) {
            if let Some(ref info) = s.execution_info {
                return info.execute_network_failure_exceeded(duration);
            }
        }

        false
    }

    /// Updates a queries migration state to `m` unless the queries migration state was
    /// `MigrationState::Unsupported`. An unsupported query cannot currently become supported once
    /// again.
    pub fn update_query_migration_state(&self, q: &Query, m: MigrationState) {
        match self.inner.get_mut(q) {
            Some(mut s) if s.migration_state != MigrationState::Unsupported => {
                // Once a query is determined to be unsupported, there is currently no going back.
                // In the future when we can support this in the query path this check should change.
                s.migration_state = m;
            }
            None => {
                let _ = self.inner.insert(
                    q.clone(),
                    QueryStatus {
                        migration_state: m,
                        execution_info: None,
                    },
                );
            }
            _ => {}
        }
    }

    /// Updates a queries status to `status` unless the queries migration state was
    /// `MigrationState::Unsupported`. An unsupported query cannot currently become supported once
    /// again.
    pub fn update_query_status(&self, q: &Query, status: QueryStatus) {
        match self.inner.get_mut(q) {
            Some(mut s) if s.migration_state != MigrationState::Unsupported => {
                s.migration_state = status.migration_state;
                s.execution_info = status.execution_info;
            }
            Some(mut s) => {
                s.execution_info = status.execution_info;
            }
            None => {
                let _ = self.inner.insert(q.clone(), status);
            }
        }
    }

    /// Returns a list of queries that currently need the be processed to determine
    /// if they should be allowed (are supported by Noria).
    pub fn pending_migration(&self) -> Vec<Query> {
        self.inner
            .iter()
            .filter(|r| matches!(r.value().migration_state, MigrationState::Pending))
            .map(|r| r.key().clone())
            .collect()
    }

    /// Returns a list of queries that have a state of [`QueryState::Successful`].
    pub fn allow_list(&self) -> QueryList {
        self.inner
            .iter()
            .filter(|r| matches!(r.value().migration_state, MigrationState::Successful))
            .map(|r| r.key().clone())
            .collect::<Vec<Query>>()
            .into()
    }

    /// Returns a list of queries that are in the deny list.
    pub fn deny_list(&self) -> QueryList {
        match self.style {
            MigrationStyle::Async | MigrationStyle::InRequestPath => self
                .inner
                .iter()
                .filter(|r| matches!(r.value().migration_state, MigrationState::Unsupported))
                .map(|r| r.key().clone())
                .collect::<Vec<Query>>()
                .into(),
            MigrationStyle::Explicit => self
                .inner
                .iter()
                .filter(|r| {
                    matches!(
                        r.value().migration_state,
                        MigrationState::Unsupported | MigrationState::Pending
                    )
                })
                .map(|r| r.key().clone())
                .collect::<Vec<Query>>()
                .into(),
        }
    }
}

/// MigrationStyle is used to communicate which style of managing migrations we have configured.
pub enum MigrationStyle {
    /// Async migrations are enabled in the adapter by passing the --async-migrations flag.
    Async,
    /// Explicit migrations are enabled in the adapter by passing the --explicit-migrations flag.
    Explicit,
    /// InRequestPath is the style of managing migrations when neither async nor explicit
    /// migrations have been enabled.
    InRequestPath,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nom_sql::SqlQuery;

    fn select_statement(s: &str) -> anyhow::Result<SelectStatement> {
        match nom_sql::parse_query(nom_sql::Dialect::MySQL, s) {
            Ok(SqlQuery::Select(s)) => Ok(s),
            _ => Err(anyhow::anyhow!("Invalid SELECT statement")),
        }
    }

    #[test]
    fn query_is_allowed() {
        let cache = QueryStatusCache::new();
        let query = select_statement("SELECT * FROM t1").unwrap();

        assert_eq!(cache.query_migration_state(&query), MigrationState::Pending);
        assert_eq!(cache.pending_migration().len(), 1);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 0);

        cache.update_query_migration_state(&query, MigrationState::Successful);
        assert_eq!(cache.pending_migration().len(), 0);
        assert_eq!(cache.allow_list().len(), 1);
        assert_eq!(cache.deny_list().len(), 0);
    }

    #[test]
    fn query_is_denied() {
        let cache = QueryStatusCache::new();
        let query = select_statement("SELECT * FROM t1").unwrap();

        assert_eq!(cache.query_migration_state(&query), MigrationState::Pending);
        assert_eq!(cache.pending_migration().len(), 1);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 0);

        cache.update_query_migration_state(&query, MigrationState::Unsupported);
        assert_eq!(cache.pending_migration().len(), 0);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 1);
    }

    #[test]
    fn query_is_inferred_denied_explicit() {
        let cache = QueryStatusCache::with_style(MigrationStyle::Explicit);
        let query = select_statement("SELECT * FROM t1").unwrap();

        assert_eq!(cache.query_migration_state(&query), MigrationState::Pending);
        assert_eq!(cache.pending_migration().len(), 1);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 1);

        cache.update_query_migration_state(&query, MigrationState::Unsupported);
        assert_eq!(cache.pending_migration().len(), 0);
        assert_eq!(cache.allow_list().len(), 0);
        assert_eq!(cache.deny_list().len(), 1);
    }
}
