use std::env;
use std::sync::Arc;
use std::time::Duration;

use dataflow::{DurabilityMode, PersistenceParameters};
use noria::consensus::{Authority, LocalAuthority, LocalAuthorityStore};
use noria::{
    metrics::client::MetricsClient,
    metrics::{DumpedMetric, DumpedMetricValue, MetricsDump},
};

use crate::metrics::{
    get_global_recorder_opt, install_global_recorder, BufferedRecorder, CompositeMetricsRecorder,
    MetricsRecorder, NoriaMetricsRecorder,
};
use crate::{Builder, Handle};

// Settle time must be longer than the leader state check interval
// when using a local authority.
pub const DEFAULT_SETTLE_TIME_MS: u64 = 1000;
pub const DEFAULT_SHARDING: usize = 2;

/// PersistenceParameters with a log_name on the form of `prefix` + timestamp,
/// avoiding collisions between separate test runs (in case an earlier panic causes clean-up to
/// fail).
pub fn get_persistence_params(prefix: &str) -> PersistenceParameters {
    PersistenceParameters {
        mode: DurabilityMode::DeleteOnExit,
        db_filename_prefix: String::from(prefix),
        ..Default::default()
    }
}

/// Builds a local worker.
pub async fn start_simple(prefix: &str) -> Handle {
    build(prefix, Some(DEFAULT_SHARDING), false).await
}

#[allow(dead_code)]
/// Builds a lock worker without sharding.
pub async fn start_simple_unsharded(prefix: &str) -> Handle {
    build(prefix, None, false).await
}

#[allow(dead_code)]
/// Builds a local worker with DEFAULT_SHARDING shards and
/// logging.
pub async fn start_simple_logging(prefix: &str) -> Handle {
    build(prefix, Some(DEFAULT_SHARDING), true).await
}

/// Builds a custom local worker with log prefix `prefix`,
/// with optional sharding and logging.
pub async fn build(prefix: &str, sharding: Option<usize>, log: bool) -> Handle {
    let authority_store = Arc::new(LocalAuthorityStore::new());
    build_custom(
        prefix,
        sharding,
        log,
        true,
        Arc::new(Authority::from(LocalAuthority::new_with_store(
            authority_store,
        ))),
        None,
        false,
    )
    .await
}

/// Builds a custom local worker.
pub async fn build_custom(
    prefix: &str,
    sharding: Option<usize>,
    log: bool,
    controller: bool,
    authority: Arc<Authority>,
    region: Option<String>,
    reader_only: bool,
) -> Handle {
    use crate::logger_pls;
    let mut builder = Builder::for_tests();
    if log {
        builder.log_with(logger_pls());
    }
    builder.set_sharding(sharding);
    builder.set_persistence(get_persistence_params(prefix));

    if reader_only {
        builder.as_reader_only();
    }

    if region.is_some() {
        builder.set_region(region.unwrap());
    }
    if controller {
        builder.start_local_custom(authority.clone()).await.unwrap()
    } else {
        builder.start(authority.clone()).await.unwrap()
    }
}

pub fn get_settle_time() -> Duration {
    let settle_time: u64 = match env::var("SETTLE_TIME") {
        Ok(value) => value.parse().unwrap(),
        Err(_) => DEFAULT_SETTLE_TIME_MS,
    };

    Duration::from_millis(settle_time)
}

/// Sleeps for either DEFAULT_SETTLE_TIME_MS milliseconds, or
/// for the value given through the SETTLE_TIME environment variable.
pub async fn sleep() {
    tokio::time::sleep(get_settle_time()).await;
}

/// Creates the metrics client for a given local deployment and initializes
/// the metrics recorder if it has not been initialized yet. Initializing the
/// metrics clears all previously recorded metrics. As such if tests are run
/// in parallel that depends on metrics, this may cause flaky metrics results.
pub async fn initialize_metrics(handle: &mut Handle) -> MetricsClient {
    unsafe {
        if get_global_recorder_opt().is_none() {
            let rec = CompositeMetricsRecorder::new();
            rec.add(MetricsRecorder::Noria(NoriaMetricsRecorder::new()));
            let bufrec = BufferedRecorder::new(rec, 1024);
            install_global_recorder(bufrec).unwrap();
        }
    }

    let mut metrics_client = MetricsClient::new(handle.c.clone().unwrap()).unwrap();
    let res = metrics_client.reset_metrics().await;
    assert!(!res.is_err());

    metrics_client
}

/// Get the counter value for `metric` from the current process. If tests
/// are run in the same process this may include values from across several
/// tests.
pub fn get_counter(metric: &str, metrics_dump: &MetricsDump) -> f64 {
    let dumped_metric: &DumpedMetric = &metrics_dump.metrics.get(metric).unwrap()[0];

    if let DumpedMetricValue::Counter(v) = dumped_metric.value {
        v
    } else {
        panic!("{} is not a counter", metric);
    }
}

/// Retrieves the value of column of a row, by passing the column name and
/// the type.
#[macro_export(local_inner_macros)]
macro_rules! get_col {
    ($row:expr, $field:expr, $into_type:ty) => {
        $row.get($field)
            .and_then(|dt| <$into_type>::try_from(dt).ok())
            .unwrap()
    };
    ($row:expr, $field:expr) => {
        $row.get($field).unwrap()
    };
}
