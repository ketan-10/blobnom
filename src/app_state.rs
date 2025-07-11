use futures::future::join_all;
use moka::future::Cache;
use mpchash::HashRing;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use std::fs;
use std::str::FromStr;
use tokio::sync::mpsc;

use crate::compression::{self, Compressor};
// Import ShardWriteOperation from shard_manager
use crate::{
    cluster::ClusterManager, config::Cfg, metrics::Metrics, shard_manager::ShardWriteOperation,
};
use bytes::Bytes;

#[derive(Hash)]
struct ShardNode(u64);

pub struct AppState {
    pub cfg: Cfg,
    pub shard_senders: Vec<mpsc::Sender<ShardWriteOperation>>,
    pub db_pools: Vec<SqlitePool>,
    /// Cache for inflight write operations to prevent race conditions in async mode.
    /// When async_write=true, SET operations return OK immediately but the actual
    /// database write happens asynchronously. This cache stores the key-value pairs
    /// for pending writes so that GET requests can return the correct data even
    /// before the write completes, preventing race conditions.
    pub inflight_cache: Cache<String, Bytes>,
    /// Cache for inflight namespaced write operations (namespace:key -> data).
    /// Same as inflight_cache but for HSET/HGET operations. The key format is
    /// "namespace:key" to avoid collisions between namespaces.
    pub inflight_hcache: Cache<String, Bytes>,
    /// Cluster manager for Redis cluster protocol
    pub cluster_manager: Option<ClusterManager>,
    pub compressor: Option<Box<dyn Compressor>>,
    /// Metrics collector
    pub metrics: Metrics,

    ring: HashRing<ShardNode>,
}

impl AppState {
    pub async fn new(
        cfg: Cfg,
        shard_receivers_out: &mut Vec<mpsc::Receiver<ShardWriteOperation>>,
    ) -> Self {
        let mut shard_senders_vec = Vec::new();
        // Clear the output vector first to ensure it's empty
        shard_receivers_out.clear();

        for _ in 0..cfg.num_shards {
            let (sender, receiver) = mpsc::channel(cfg.batch_size.unwrap_or(100));
            shard_senders_vec.push(sender);
            shard_receivers_out.push(receiver); // Populate the output vector with receivers
        }

        // Create data directory if it doesn't exist
        fs::create_dir_all(&cfg.data_dir).expect("Failed to create data directory");

        let mut db_pools_futures = vec![];
        for i in 0..cfg.num_shards {
            let data_dir = cfg.data_dir.clone();
            let db_path = format!("{}/shard_{}.db", data_dir, i);

            let mut connect_options =
                SqliteConnectOptions::from_str(&format!("sqlite:{}", db_path))
                    .expect(&format!(
                        "Failed to parse connection string for shard {}",
                        i
                    ))
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .busy_timeout(std::time::Duration::from_millis(5000));

            // These PRAGMAs are often set for performance with WAL mode.
            // `synchronous = OFF` is safe except for power loss.
            // `cache_size` is negative to indicate KiB, so -4000 is 4MB.
            // `temp_store = MEMORY` avoids disk I/O for temporary tables.
            connect_options = connect_options
                .pragma("synchronous", "OFF")
                .pragma("cache_size", "-100000") // 4MB cache per shard
                .pragma("temp_store", "MEMORY");

            db_pools_futures.push(sqlx::SqlitePool::connect_with(connect_options))
        }

        let db_pool_results: Vec<Result<SqlitePool, sqlx::Error>> =
            join_all(db_pools_futures).await;
        let db_pools: Vec<SqlitePool> = db_pool_results
            .into_iter()
            .enumerate()
            .map(|(i, res)| {
                res.unwrap_or_else(|e| panic!("Failed to connect to shard {} DB: {}", i, e))
            })
            .collect();

        for (i, pool) in db_pools.iter().enumerate() {
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS blobs (
                    key TEXT PRIMARY KEY,
                    data BLOB,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    expires_at INTEGER,
                    version INTEGER NOT NULL DEFAULT 0
                )",
            )
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("Failed to create table in shard {} DB: {}", i, e));

            // Create index on expires_at for efficient expiry queries
            sqlx::query(
                "CREATE INDEX IF NOT EXISTS idx_expires_at ON blobs(expires_at) WHERE expires_at IS NOT NULL",
            )
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("Failed to create expires_at index in shard {} DB: {}", i, e));
        }

        // Create caches for inflight operations
        // Use a reasonable capacity - adjust based on expected load
        let inflight_cache = Cache::new(10_000);
        let inflight_hcache = Cache::new(10_000);

        // Initialize cluster manager if clustering is enabled
        let cluster_manager = if let Some(ref cluster_config) = cfg.cluster {
            if cluster_config.enabled {
                let gossip_bind_addr = format!("0.0.0.0:{}", cluster_config.port)
                    .parse()
                    .expect("Invalid cluster bind address");

                let redis_addr = cfg
                    .addr
                    .as_deref()
                    .unwrap_or("0.0.0.0:6379")
                    .parse()
                    .expect("Invalid Redis server address");

                match ClusterManager::new(cluster_config, gossip_bind_addr, redis_addr).await {
                    Ok(manager) => {
                        tracing::info!("Cluster manager initialized successfully");
                        Some(manager)
                    }
                    Err(e) => {
                        tracing::error!("Failed to initialize cluster manager: {}", e);
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let compressor = match &cfg.storage_compression {
            Some(comp) if comp.enabled => Some(compression::init_compression(comp.clone())),
            _ => None,
        };

        // Initialize metrics
        let metrics = Metrics::new();

        let ring = HashRing::new();
        for i in 0..cfg.num_shards {
            ring.add(ShardNode(i as u64));
        }

        AppState {
            cfg,
            shard_senders: shard_senders_vec,
            db_pools,
            inflight_cache,
            inflight_hcache,
            cluster_manager,
            compressor,
            metrics,
            ring,
        }
    }

    pub fn get_shard(&self, key: &str) -> usize {
        let token = self.ring.node(&key).unwrap();
        token.node().0 as usize
    }

    /// Get a namespaced cache key for HGET/HSET operations
    pub fn namespaced_key(&self, namespace: &str, key: &str) -> String {
        format!("{}:{}", namespace, key)
    }
}
