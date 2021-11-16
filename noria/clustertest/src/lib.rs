mod server;

#[cfg(test)]
mod readyset;
#[cfg(test)]
mod readyset_mysql;
#[cfg(test)]
mod utils;

use anyhow::{anyhow, Result};
use futures::executor;
use mysql::prelude::Queryable;
use noria::consensus::AuthorityType;
use noria::metrics::client::MetricsClient;
use noria::ControllerHandle;
use rand::Rng;
use serde::Deserialize;
use server::{NoriaMySQLRunner, NoriaServerRunner, ProcessHandle};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use url::Url;

/// The set of environment variables that need to be set for the
/// tests to run. Each variable is the upper case of their respective,
/// struct variable name, i.e. AUTHORITY_ADDRESS.
#[derive(Deserialize, Debug)]
struct Env {
    #[serde(default = "default_authority_address")]
    authority_address: String,
    #[serde(default = "default_authority")]
    authority: String,
    #[serde(default = "default_binary_path")]
    binary_path: PathBuf,
    #[serde(default = "default_mysql_host")]
    mysql_host: String,
    #[serde(default = "default_root_password")]
    mysql_root_password: String,
}

fn default_authority_address() -> String {
    "127.0.0.1:8500".to_string()
}

fn default_mysql_host() -> String {
    "127.0.0.1".to_string()
}

fn default_authority() -> String {
    "consul".to_string()
}

fn default_binary_path() -> PathBuf {
    // Convert from <dir>/noria/clustertest to <dir>/target/debug.
    let mut path: PathBuf = std::env::var("CARGO_MANIFEST_DIR").unwrap().into();
    path.pop();
    path.pop();
    path.push("target/debug");
    path
}

fn default_root_password() -> String {
    "noria".to_string()
}

/// Source of the noria binaries.
pub struct NoriaBinarySource {
    /// Path to a built noria-server on the local machine.
    pub noria_server: PathBuf,
    /// Optional path to noria-mysql on the local machine. noria-mysql
    /// may not be included in the build.
    pub noria_mysql: Option<PathBuf>,
}

/// Parameters for a single noria-server instance.
#[derive(Clone)]
pub struct ServerParams {
    /// A server's region string, passed in via --region.
    region: Option<String>,
    /// THe volume id of the server, passed in via --volume-id.
    volume_id: Option<String>,
}

impl ServerParams {
    pub fn default() -> Self {
        Self {
            region: None,
            volume_id: None,
        }
    }

    pub fn with_region(mut self, region: &str) -> Self {
        self.region = Some(region.to_string());
        self
    }

    pub fn with_volume(mut self, volume: &str) -> Self {
        self.volume_id = Some(volume.to_string());
        self
    }
}

/// Set of parameters defining an entire cluster's topology.
pub struct DeploymentParams {
    /// Name of the cluster, cluster resources will be prefixed
    /// with this name.
    name: String,
    /// Source of the binaries.
    noria_binaries: NoriaBinarySource,
    /// Number of shards for dataflow nodes.
    sharding: Option<usize>,
    /// The primary region of the noria cluster.
    primary_region: Option<String>,
    /// Parameters for the set of noria-server instances in the deployment.
    servers: Vec<ServerParams>,
    /// Deploy the mysql adapter.
    mysql_adapter: bool,
    /// Deploy mysql and use binlog replication.
    mysql: bool,
    /// The type of authority to use for cluster management.
    authority: AuthorityType,
    /// The address of the authority.
    authority_address: String,
    /// The address of the mysql host.
    mysql_host: String,
    /// The root password for the mysql db.
    mysql_root_password: String,
    /// Is live QCA enabled on the adapter.
    live_qca_interval: Option<u64>,
}

impl DeploymentParams {
    pub fn new(name: &str) -> Self {
        let env = envy::from_env::<Env>().unwrap();

        let mut noria_server_path = env.binary_path.clone();
        noria_server_path.push("noria-server");

        let mut noria_mysql_path = env.binary_path;
        noria_mysql_path.push("noria-mysql");

        // Append the deployment name with a random number to prevent state collisions
        // on test repeats with failed teardowns.
        let mut rng = rand::thread_rng();
        let name = name.to_string() + &rng.gen::<u32>().to_string();

        Self {
            name,
            noria_binaries: NoriaBinarySource {
                noria_server: noria_server_path,
                noria_mysql: Some(noria_mysql_path),
            },
            sharding: None,
            primary_region: None,
            servers: vec![],
            mysql_adapter: false,
            mysql: false,
            authority: AuthorityType::from_str(&env.authority).unwrap(),
            authority_address: env.authority_address,
            mysql_host: env.mysql_host,
            mysql_root_password: env.mysql_root_password,
            live_qca_interval: None,
        }
    }

    pub fn set_binary_source(&mut self, source: NoriaBinarySource) {
        self.noria_binaries = source;
    }

    pub fn set_sharding(&mut self, shards: usize) {
        self.sharding = Some(shards);
    }

    pub fn set_primary_region(&mut self, region: &str) {
        self.primary_region = Some(region.to_string());
    }

    pub fn add_server(&mut self, server: ServerParams) {
        self.servers.push(server);
    }

    pub fn deploy_mysql_adapter(&mut self) {
        self.mysql_adapter = true;
    }

    pub fn deploy_mysql(&mut self) {
        self.mysql = true;
    }

    pub fn set_authority(&mut self, authority: AuthorityType) {
        self.authority = authority;
    }

    pub fn set_authority_address(&mut self, authority_address: String) {
        self.authority_address = authority_address;
    }

    pub fn enable_live_qca(&mut self, interval_ms: u64) {
        self.live_qca_interval = Some(interval_ms);
    }
}

/// A handle to a single server in the deployment.
pub struct ServerHandle {
    /// The external address of the server.
    pub addr: Url,
    /// The parameters used to create the server.
    pub params: ServerParams,
    /// The local process the server is running in.
    pub process: ProcessHandle,
}

/// A handle to a mysql-adapter instance in the deployment.
pub struct MySQLAdapterHandle {
    /// The mysql connection string of the adapter.
    pub conn_str: String,
    /// The local process the adapter is running in.
    pub process: ProcessHandle,
}

/// A handle to a deployment created with `start_multi_process`.
pub struct DeploymentHandle {
    /// A handle to the current controller of the deployment.
    pub handle: ControllerHandle,
    /// Metrics client for aggregating metrics across the deployment.
    pub metrics: MetricsClient,
    /// Map from a noria server's address to a handle to the server.
    pub noria_server_handles: HashMap<Url, ServerHandle>,
    /// The name of the deployment, cluster resources are prefixed
    /// by `name`.
    name: String,
    /// The authority connect string for the deployment.
    authority_addr: String,
    /// The authority type for the deployment.
    authority: AuthorityType,
    /// The MySql connect string for the deployment.
    mysql_addr: Option<String>,
    /// A handle to each noria server in the deployment.
    /// True if this deployment has already been torn down.
    shutdown: bool,
    /// The paths to the binaries for the deployment.
    noria_binaries: NoriaBinarySource,
    /// Dataflow sharding for new servers.
    sharding: Option<usize>,
    /// The primary region of the deployment.
    primary_region: Option<String>,
    /// Next new server port.
    port: u16,
    /// Holds a handle to the mysql adapter if this deployment includes
    /// a mysql adapter.
    mysql_adapter: Option<MySQLAdapterHandle>,
}

impl DeploymentHandle {
    /// Start a new noria-server instance in the deployment.
    pub async fn start_server(&mut self, params: ServerParams) -> anyhow::Result<Url> {
        let port = get_next_good_port(Some(self.port));
        self.port = port;
        let handle = start_server(
            &params,
            &self.noria_binaries.noria_server,
            &self.name,
            self.sharding,
            self.primary_region.as_ref(),
            &self.authority_addr,
            &self.authority.to_string(),
            port,
            self.mysql_addr.as_ref(),
        )?;
        let server_addr = handle.addr.clone();
        self.noria_server_handles
            .insert(server_addr.clone(), handle);

        // Wait until the worker has been created and is visible over rpc.
        wait_until_worker_count(
            &mut self.handle,
            Duration::from_secs(15),
            self.noria_server_handles.len(),
        )
        .await?;
        Ok(server_addr)
    }

    /// Kill an existing noria-server instance in the deployment referenced
    /// by `ServerHandle`.
    pub async fn kill_server(&mut self, server_addr: &Url) -> anyhow::Result<()> {
        if !self.noria_server_handles.contains_key(server_addr) {
            return Err(anyhow!("Server handle does not exist in deployment"));
        }

        let mut handle = self.noria_server_handles.remove(server_addr).unwrap();
        handle.process.kill()?;

        // Wait until the server is no longer visible in the deployment.
        wait_until_worker_count(
            &mut self.handle,
            Duration::from_secs(45),
            self.noria_server_handles.len(),
        )
        .await?;

        Ok(())
    }

    /// Tears down any resources associated with the deployment.
    pub async fn teardown(&mut self) -> anyhow::Result<()> {
        if self.shutdown {
            return Ok(());
        }

        // Clean up the existing mysql state.
        if let Some(mysql_addr) = &self.mysql_addr {
            let opts = mysql::Opts::from_url(mysql_addr).unwrap();
            let mut conn = mysql::Conn::new(opts).unwrap();
            conn.query_drop(format!("DROP DATABASE {};", &self.name))?;
        }

        // Drop any errors on failure to kill so we complete
        // cleanup.
        for h in &mut self.noria_server_handles {
            let _ = h.1.process.kill();
        }
        if let Some(adapter_handle) = &mut self.mysql_adapter {
            let _ = adapter_handle.process.kill();
        }
        std::fs::remove_dir_all(&get_log_path(&self.name))?;

        self.shutdown = true;
        Ok(())
    }

    pub fn server_addrs(&self) -> Vec<Url> {
        self.noria_server_handles.keys().cloned().collect()
    }

    pub fn server_handles(&mut self) -> &mut HashMap<Url, ServerHandle> {
        &mut self.noria_server_handles
    }

    pub fn mysql_connection_str(&self) -> Option<String> {
        self.mysql_adapter.as_ref().map(|h| h.conn_str.clone())
    }

    pub fn mysql_db_str(&self) -> Option<String> {
        self.mysql_addr.clone()
    }
}

impl Drop for DeploymentHandle {
    // Attempt to clean up any resources used by the DeploymentHandle. Drop
    // will be called on test panics allowing resources to be cleaned up.
    // TODO(justin): This does not always work if a test does not cleanup
    // with teardown explicitly, leading to noria-server instances living.
    #[allow(unused_must_use)]
    fn drop(&mut self) {
        executor::block_on(self.teardown());
    }
}

// Queries the number of workers every half second until `max_wait`.
async fn wait_until_worker_count(
    handle: &mut ControllerHandle,
    max_wait: Duration,
    num_workers: usize,
) -> Result<()> {
    if num_workers == 0 {
        return Ok(());
    }

    let start = Instant::now();
    loop {
        let now = Instant::now();
        if (now - start) > max_wait {
            break;
        }

        if let Ok(workers) = handle.healthy_workers().await {
            if workers.len() == num_workers {
                return Ok(());
            }
        }

        sleep(Duration::from_millis(500)).await;
    }

    Err(anyhow!("Exceeded maximum time to wait for workers"))
}

/// Returns the path `temp_dir()`/deployment_name.
fn get_log_path(deployment_name: &str) -> PathBuf {
    std::env::temp_dir().join(deployment_name)
}

#[allow(clippy::too_many_arguments)]
fn start_server(
    server_params: &ServerParams,
    noria_server_path: &Path,
    deployment_name: &str,
    sharding: Option<usize>,
    primary_region: Option<&String>,
    authority_addr: &str,
    authority: &str,
    port: u16,
    mysql: Option<&String>,
) -> Result<ServerHandle> {
    let mut runner = NoriaServerRunner::new(noria_server_path);
    runner.set_deployment(deployment_name);
    runner.set_external_port(port);
    runner.set_authority_addr(authority_addr);
    runner.set_authority(authority);
    if let Some(shard) = sharding {
        runner.set_shards(shard);
    }
    let region = server_params.region.as_ref();
    if let Some(region) = region {
        runner.set_region(region);
    }
    if let Some(region) = primary_region.as_ref() {
        runner.set_primary_region(region);
    }
    if let Some(volume) = server_params.volume_id.as_ref() {
        runner.set_volume_id(volume);
    }
    if let Some(mysql) = mysql {
        runner.set_mysql(mysql);
    }
    let log_path = get_log_path(deployment_name).join(port.to_string());
    std::fs::create_dir_all(&log_path)?;
    runner.set_log_dir(&log_path);

    let addr = Url::parse(&format!("http://127.0.0.1:{}", port)).unwrap();
    Ok(ServerHandle {
        addr,
        process: runner.start()?,
        params: server_params.clone(),
    })
}

// TODO(justin): Wrap these parameters.
#[allow(clippy::too_many_arguments)]
fn start_mysql_adapter(
    noria_mysql_path: &Path,
    deployment_name: &str,
    authority_addr: &str,
    authority: &str,
    port: u16,
    metrics_port: u16,
    mysql: Option<&String>,
    live_qca_interval: Option<u64>,
) -> Result<ProcessHandle> {
    let mut runner = NoriaMySQLRunner::new(noria_mysql_path);
    runner.set_deployment(deployment_name);
    runner.set_port(port);
    runner.set_metrics_port(metrics_port);
    runner.set_authority_addr(authority_addr);
    runner.set_authority(authority);

    if let Some(interval) = live_qca_interval {
        runner.set_live_qca(interval);
    }

    if let Some(mysql) = mysql {
        runner.set_mysql(mysql);
    }

    runner.start()
}

/// Checks the set of deployment params for invalid configurations
pub fn check_deployment_params(params: &DeploymentParams) -> anyhow::Result<()> {
    match &params.primary_region {
        Some(pr) => {
            // If the primary region is set, at least one server should match that
            // region.
            if params
                .servers
                .iter()
                .all(|s| s.region.as_ref().filter(|region| region == &pr).is_none())
            {
                return Err(anyhow!(
                    "Primary region specified, but no servers match
                    the region."
                ));
            }
        }
        None => {
            // If the primary region is not set, servers should not include a `region`
            // parameter. Otherwise, a controller will not be elected.
            if params.servers.iter().any(|s| s.region.is_some()) {
                return Err(anyhow!(
                    "Servers have region without a deployment primary region"
                ));
            }
        }
    }
    Ok(())
}

/// Finds the next available port after `port` (if supplied).
/// Otherwise, it returns a random available port in the range of 20000-60000.
fn get_next_good_port(port: Option<u16>) -> u16 {
    let mut port = port.map(|p| p + 1).unwrap_or_else(|| {
        let mut rng = rand::thread_rng();
        rng.gen_range(20000..60000)
    });
    while !port_scanner::local_port_available(port) {
        port += 1;
    }
    port
}

/// Used to create a multi_process test deployment. This deployment
/// connects to an authority for cluster management, and deploys a
/// set of noria-servers. `params` can be used to setup the topology
/// of the deployment for testing.
pub async fn start_multi_process(params: DeploymentParams) -> anyhow::Result<DeploymentHandle> {
    check_deployment_params(&params)?;
    let mut port = get_next_good_port(None);
    // If this deployment includes binlog replication and a mysql instance.
    let mut mysql_addr = None;
    if params.mysql {
        // TODO(justin): Parameterize port.
        let addr = format!(
            "mysql://root:{}@{}:3306",
            &params.mysql_root_password, &params.mysql_host
        );
        let opts = mysql::Opts::from_url(&addr).unwrap();
        let mut conn = mysql::Conn::new(opts).unwrap();
        let _ = conn
            .query_drop(format!("CREATE DATABASE {};", &params.name))
            .unwrap();
        mysql_addr = Some(format!("{}/{}", &addr, &params.name));
    }

    // Create the noria-server instances.
    let mut handles = HashMap::new();
    for server in &params.servers {
        port = get_next_good_port(Some(port));
        let handle = start_server(
            server,
            &params.noria_binaries.noria_server,
            &params.name,
            params.sharding,
            params.primary_region.as_ref(),
            &params.authority_address,
            &params.authority.to_string(),
            port,
            mysql_addr.as_ref(),
        )?;

        handles.insert(handle.addr.clone(), handle);
    }

    let authority = params
        .authority
        .to_authority(&params.authority_address, &params.name)
        .await;
    let mut handle = ControllerHandle::new(authority).await;
    wait_until_worker_count(&mut handle, Duration::from_secs(15), params.servers.len()).await?;

    // Duplicate the authority and handle creation as the metrics client
    // owns its own handle.
    let metrics_authority = params
        .authority
        .to_authority(&params.authority_address, &params.name)
        .await;
    let metrics_handle = ControllerHandle::new(metrics_authority).await;
    let metrics = MetricsClient::new(metrics_handle).unwrap();

    // Start a MySQL adapter instance.
    let mysql_adapter_handle = if params.mysql_adapter || params.mysql {
        // TODO(justin): Turn this into a stateful object.
        port = get_next_good_port(Some(port));
        let metrics_port = get_next_good_port(Some(port));
        let process = start_mysql_adapter(
            params.noria_binaries.noria_mysql.as_ref().unwrap(),
            &params.name,
            &params.authority_address,
            &params.authority.to_string(),
            port,
            metrics_port,
            mysql_addr.as_ref(),
            params.live_qca_interval,
        )?;
        // Sleep to give the adapter time to startup.
        sleep(Duration::from_millis(500)).await;
        Some(MySQLAdapterHandle {
            conn_str: format!("mysql://127.0.0.1:{}", port),
            process,
        })
    } else {
        None
    };

    Ok(DeploymentHandle {
        handle,
        metrics,
        name: params.name.clone(),
        authority_addr: params.authority_address,
        authority: params.authority,
        mysql_addr,
        noria_server_handles: handles,
        shutdown: false,
        noria_binaries: params.noria_binaries,
        sharding: params.sharding,
        primary_region: params.primary_region,
        port,
        mysql_adapter: mysql_adapter_handle,
    })
}

// These tests currently require that a docker daemon is already setup
// and accessible by the user calling cargo test. As these tests interact
// with a stateful external component, the docker daemon, each test is
// responsible for cleaning up its own external state.
#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    // Verifies that the wrappers that create and teardown the deployment
    // correctly setup zookeeper containers.
    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn clustertest_startup_teardown_test() {
        let cluster_name = "ct_startup_teardown";

        let mut deployment = DeploymentParams::new(cluster_name);
        deployment.add_server(ServerParams::default());
        deployment.add_server(ServerParams::default());

        let deployment = start_multi_process(deployment).await;
        assert!(
            !deployment.is_err(),
            "Error starting deployment: {}",
            deployment.err().unwrap()
        );

        let mut deployment = deployment.unwrap();

        // Check we received a metrics dump from each client.
        let metrics = deployment.metrics.get_metrics().await.unwrap();
        assert_eq!(metrics.len(), 2);

        // Check that the controller can respond to an rpc.
        let workers = deployment.handle.healthy_workers().await.unwrap();
        assert_eq!(workers.len(), 2);

        let res = deployment.teardown().await;
        assert!(
            !res.is_err(),
            "Error tearing down deployment: {}",
            res.err().unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn clustertest_minimal() {
        let cluster_name = "ct_minimal";
        let mut deployment = DeploymentParams::new(cluster_name);
        deployment.add_server(ServerParams::default());
        deployment.add_server(ServerParams::default());

        let mut deployment = start_multi_process(deployment).await.unwrap();
        deployment.teardown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn clustertest_multiregion() {
        let cluster_name = "ct_multiregion";
        let mut deployment = DeploymentParams::new(cluster_name);
        deployment.set_primary_region("r1");
        deployment.add_server(ServerParams::default().with_region("r1"));
        deployment.add_server(ServerParams::default().with_region("r2"));

        let mut deployment = start_multi_process(deployment).await.unwrap();
        deployment.teardown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn clustertest_server_management() {
        let cluster_name = "ct_server_management";
        let mut deployment = DeploymentParams::new(cluster_name);
        deployment.set_primary_region("r1");
        deployment.add_server(ServerParams::default().with_region("r1"));
        deployment.add_server(ServerParams::default().with_region("r2"));

        let mut deployment = start_multi_process(deployment).await.unwrap();

        // Check that we currently have two workers.
        assert_eq!(deployment.handle.healthy_workers().await.unwrap().len(), 2);

        // Start up a new server.
        let server_handle = deployment
            .start_server(ServerParams::default().with_region("r3"))
            .await
            .unwrap();
        assert_eq!(deployment.handle.healthy_workers().await.unwrap().len(), 3);

        // Now kill that server we started up.
        deployment.kill_server(&server_handle).await.unwrap();
        assert_eq!(deployment.handle.healthy_workers().await.unwrap().len(), 2);

        deployment.teardown().await.unwrap();
    }

    #[tokio::test]
    async fn clustertest_no_server_in_primary_region_test() {
        let mut deployment = DeploymentParams::new("fake_cluster");

        deployment.set_primary_region("r1");
        deployment.add_server(ServerParams::default().with_region("r2"));
        deployment.add_server(ServerParams::default().with_region("r3"));
        assert!(start_multi_process(deployment).await.is_err());
    }

    #[tokio::test]
    async fn clustertest_server_region_without_primary_region() {
        let mut deployment = DeploymentParams::new("fake_cluster_2");

        deployment.add_server(ServerParams::default().with_region("r1"));
        deployment.add_server(ServerParams::default().with_region("r2"));

        assert!(start_multi_process(deployment).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn clustertest_with_binlog() {
        let cluster_name = "ct_with_binlog";
        let mut deployment = DeploymentParams::new(cluster_name);
        deployment.add_server(ServerParams::default());
        deployment.add_server(ServerParams::default());
        deployment.deploy_mysql();

        let mut deployment = start_multi_process(deployment).await.unwrap();

        // Check that we currently have two workers.
        assert_eq!(deployment.handle.healthy_workers().await.unwrap().len(), 2);
        deployment.teardown().await.unwrap();
    }
}
