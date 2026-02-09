//! Redis queue for review jobs.

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use tracing::{debug, error, info};

use crate::payload::JobPayload;

const QUEUE_KEY: &str = "claude-agent:review-queue";
const PROCESSING_KEY: &str = "claude-agent:processing";
const FAILED_KEY: &str = "claude-agent:failed";

/// Queue item with metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub payload: JobPayload,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub attempts: u32,
}

impl QueueItem {
    pub fn new(payload: impl Into<JobPayload>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            payload: payload.into(),
            created_at: chrono::Utc::now(),
            attempts: 0,
        }
    }
}

/// Redis-backed queue for review jobs.
#[derive(Clone)]
pub struct Queue {
    conn: ConnectionManager,
}

impl Queue {
    pub async fn new(redis_url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(redis_url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self { conn })
    }

    /// Push a job payload to the queue.
    pub async fn push(&self, payload: impl Into<JobPayload>) -> Result<String, redis::RedisError> {
        let item = QueueItem::new(payload);
        let id = item.id.clone();
        let description = item.payload.description();
        let json = serde_json::to_string(&item).unwrap();

        let mut conn = self.conn.clone();
        conn.rpush::<_, _, ()>(QUEUE_KEY, &json).await?;

        info!(id = %id, job = %description, "Queued job");
        Ok(id)
    }

    /// Pop the next item from the queue (blocking).
    pub async fn pop(&self, timeout_secs: u64) -> Result<Option<QueueItem>, redis::RedisError> {
        let mut conn = self.conn.clone();

        // BLPOP returns (key, value) or None on timeout
        let result: Option<(String, String)> = conn
            .blpop(QUEUE_KEY, timeout_secs as f64)
            .await?;

        match result {
            Some((_, json)) => {
                let item: QueueItem = serde_json::from_str(&json).unwrap();
                debug!(id = %item.id, "Popped review job");
                Ok(Some(item))
            }
            None => Ok(None),
        }
    }

    /// Mark an item as processing.
    pub async fn mark_processing(&self, item: &QueueItem) -> Result<(), redis::RedisError> {
        let mut conn = self.conn.clone();
        let json = serde_json::to_string(item).unwrap();
        conn.hset::<_, _, _, ()>(PROCESSING_KEY, &item.id, &json)
            .await?;
        Ok(())
    }

    /// Mark an item as completed (remove from processing).
    pub async fn mark_completed(&self, id: &str) -> Result<(), redis::RedisError> {
        let mut conn = self.conn.clone();
        conn.hdel::<_, _, ()>(PROCESSING_KEY, id).await?;
        info!(id = %id, "Marked review job completed");
        Ok(())
    }

    /// Mark an item as failed.
    pub async fn mark_failed(
        &self,
        mut item: QueueItem,
        error: &str,
    ) -> Result<(), redis::RedisError> {
        let mut conn = self.conn.clone();

        // Remove from processing
        conn.hdel::<_, _, ()>(PROCESSING_KEY, &item.id).await?;

        // Add to failed with error info
        item.attempts += 1;
        let failed = FailedItem {
            item,
            error: error.to_string(),
            failed_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&failed).unwrap();
        conn.rpush::<_, _, ()>(FAILED_KEY, &json).await?;

        error!(id = %failed.item.id, error = %error, "Marked review job failed");
        Ok(())
    }

    /// Get queue length.
    #[allow(clippy::len_without_is_empty)]
    pub async fn len(&self) -> Result<usize, redis::RedisError> {
        let mut conn = self.conn.clone();
        let len: usize = conn.llen(QUEUE_KEY).await?;
        Ok(len)
    }

    /// Get number of processing items.
    pub async fn processing_count(&self) -> Result<usize, redis::RedisError> {
        let mut conn = self.conn.clone();
        let len: usize = conn.hlen(PROCESSING_KEY).await?;
        Ok(len)
    }

    /// Get number of failed items.
    pub async fn failed_count(&self) -> Result<usize, redis::RedisError> {
        let mut conn = self.conn.clone();
        let len: usize = conn.llen(FAILED_KEY).await?;
        Ok(len)
    }

    /// List failed items.
    pub async fn list_failed(&self, limit: usize) -> Result<Vec<FailedItem>, redis::RedisError> {
        let mut conn = self.conn.clone();
        let items: Vec<String> = conn
            .lrange(FAILED_KEY, 0, (limit as isize) - 1)
            .await?;

        Ok(items
            .into_iter()
            .filter_map(|json| serde_json::from_str(&json).ok())
            .collect())
    }

    /// Retry a failed item by moving it back to the queue.
    pub async fn retry_failed(&self, id: &str) -> Result<bool, redis::RedisError> {
        let mut conn = self.conn.clone();

        // Get all failed items
        let items: Vec<String> = conn.lrange(FAILED_KEY, 0, -1).await?;

        for json in &items {
            let failed: FailedItem = match serde_json::from_str(json) {
                Ok(f) => f,
                Err(_) => continue,
            };

            if failed.item.id == id {
                // Remove from failed list
                conn.lrem::<_, _, ()>(FAILED_KEY, 1, json).await?;

                // Re-queue
                let item_json = serde_json::to_string(&failed.item).unwrap();
                conn.rpush::<_, _, ()>(QUEUE_KEY, &item_json).await?;

                info!(id = %id, "Retried failed job");
                return Ok(true);
            }
        }

        Ok(false)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FailedItem {
    pub item: QueueItem,
    pub error: String,
    pub failed_at: chrono::DateTime<chrono::Utc>,
}
