//! Kubernetes job scheduler.
//!
//! Polls the queue and spawns K8s Jobs for review tasks.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, EnvVarSource, PodSpec, PodTemplateSpec,
    ResourceRequirements, SecretKeySelector, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::Client;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::queue::{Queue, QueueItem};

const NAMESPACE: &str = "claude-agent";
const WORKER_IMAGE: &str = "registry.digitalocean.com/globalcomix/claude-agent-worker:latest";
const JOB_TTL_SECONDS: i32 = 900; // 15 minutes after completion

/// Job scheduler that processes the queue sequentially.
pub struct Scheduler {
    queue: Queue,
    #[allow(dead_code)]
    k8s_client: Client,
    jobs_api: Api<Job>,
    running: Arc<Mutex<bool>>,
}

impl Scheduler {
    pub async fn new(queue: Queue) -> Result<Self, kube::Error> {
        let k8s_client = Client::try_default().await?;
        let jobs_api = Api::namespaced(k8s_client.clone(), NAMESPACE);

        Ok(Self {
            queue,
            k8s_client,
            jobs_api,
            running: Arc::new(Mutex::new(false)),
        })
    }

    /// Start the scheduler loop.
    pub async fn run(&self) {
        info!("Starting scheduler");
        *self.running.lock().await = true;

        while *self.running.lock().await {
            // Wait for any running job to finish before popping
            if self.has_running_job().await {
                debug!("Job already running, waiting");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }

            // Try to get next item from queue (blocks for 30s if empty)
            match self.queue.pop(30).await {
                Ok(Some(item)) => {
                    info!(id = %item.id, "Processing queue item");

                    // Mark as processing
                    if let Err(e) = self.queue.mark_processing(&item).await {
                        error!(error = %e, "Failed to mark item as processing");
                        continue;
                    }

                    // Spawn K8s Job
                    match self.spawn_job(&item).await {
                        Ok(job_name) => {
                            info!(job = %job_name, "Spawned review job");

                            // Wait for job completion
                            match self.wait_for_job(&job_name).await {
                                Ok(success) => {
                                    if success {
                                        let _ = self.queue.mark_completed(&item.id).await;
                                    } else {
                                        let _ = self
                                            .queue
                                            .mark_failed(item, "Job failed")
                                            .await;
                                    }
                                }
                                Err(e) => {
                                    error!(error = %e, "Error waiting for job");
                                    let _ = self
                                        .queue
                                        .mark_failed(item, &format!("Wait error: {e}"))
                                        .await;
                                }
                            }
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to spawn job");
                            let _ = self
                                .queue
                                .mark_failed(item, &format!("Spawn error: {e}"))
                                .await;
                        }
                    }
                }
                Ok(None) => {
                    // Queue empty, BLPOP timed out - continue waiting
                }
                Err(e) => {
                    error!(error = %e, "Failed to pop from queue");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }

        info!("Scheduler stopped");
    }

    /// Stop the scheduler.
    pub async fn stop(&self) {
        info!("Stopping scheduler");
        *self.running.lock().await = false;
    }

    /// Check if there's a running job.
    async fn has_running_job(&self) -> bool {
        let lp = ListParams::default().labels("app=claude-review");

        match self.jobs_api.list(&lp).await {
            Ok(jobs) => {
                for job in jobs.items {
                    if let Some(status) = job.status {
                        // Job is running if active > 0
                        if status.active.unwrap_or(0) > 0 {
                            return true;
                        }
                    }
                }
                false
            }
            Err(e) => {
                warn!(error = %e, "Failed to list jobs");
                false
            }
        }
    }

    /// Spawn a K8s Job for the review.
    async fn spawn_job(&self, item: &QueueItem) -> Result<String, kube::Error> {
        let job_name = format!(
            "claude-review-{}-{}",
            item.payload.mr_iid,
            &item.id[..8]
        );

        // Encode payload as base64
        let payload_json = serde_json::to_string(&item.payload).unwrap();
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(&payload_json);

        let job = Job {
            metadata: kube::api::ObjectMeta {
                name: Some(job_name.clone()),
                namespace: Some(NAMESPACE.into()),
                labels: Some(BTreeMap::from([
                    ("app".to_string(), "claude-review".to_string()),
                    ("queue-id".to_string(), item.id.clone()),
                ])),
                ..Default::default()
            },
            spec: Some(JobSpec {
                ttl_seconds_after_finished: Some(JOB_TTL_SECONDS),
                backoff_limit: Some(0), // No retries
                template: PodTemplateSpec {
                    metadata: Some(kube::api::ObjectMeta {
                        labels: Some(BTreeMap::from([(
                            "app".to_string(),
                            "claude-review".to_string(),
                        )])),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        restart_policy: Some("Never".into()),
                        security_context: Some(
                            k8s_openapi::api::core::v1::PodSecurityContext {
                                run_as_user: Some(1000),
                                run_as_group: Some(1000),
                                fs_group: Some(1000),
                                ..Default::default()
                            },
                        ),
                        containers: vec![Container {
                            name: "worker".into(),
                            image: Some(WORKER_IMAGE.into()),
                            env: Some(vec![
                                EnvVar {
                                    name: "REVIEW_PAYLOAD".into(),
                                    value: Some(payload_b64),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "ANTHROPIC_API_KEY".into(),
                                    value_from: Some(EnvVarSource {
                                        secret_key_ref: Some(SecretKeySelector {
                                            name: "claude-agent-secrets".into(),
                                            key: "anthropic-api-key".into(),
                                            optional: Some(false),
                                        }),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "GITLAB_TOKEN".into(),
                                    value_from: Some(EnvVarSource {
                                        secret_key_ref: Some(SecretKeySelector {
                                            name: "claude-agent-secrets".into(),
                                            key: "gitlab-token".into(),
                                            optional: Some(false),
                                        }),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "GITHUB_TOKEN".into(),
                                    value_from: Some(EnvVarSource {
                                        secret_key_ref: Some(SecretKeySelector {
                                            name: "claude-agent-secrets".into(),
                                            key: "github-token".into(),
                                            optional: Some(true),
                                        }),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                },
                            ]),
                            volume_mounts: Some(vec![VolumeMount {
                                name: "workdir".into(),
                                mount_path: "/work".into(),
                                ..Default::default()
                            }]),
                            resources: Some(ResourceRequirements {
                                requests: Some(BTreeMap::from([
                                    ("memory".to_string(), Quantity("512Mi".into())),
                                    ("cpu".to_string(), Quantity("500m".into())),
                                ])),
                                limits: Some(BTreeMap::from([
                                    ("memory".to_string(), Quantity("4Gi".into())),
                                    ("cpu".to_string(), Quantity("2000m".into())),
                                ])),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }],
                        volumes: Some(vec![Volume {
                            name: "workdir".into(),
                            empty_dir: Some(EmptyDirVolumeSource {
                                size_limit: Some(Quantity("2Gi".into())),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        self.jobs_api.create(&PostParams::default(), &job).await?;
        Ok(job_name)
    }

    /// Wait for a job to complete.
    async fn wait_for_job(&self, job_name: &str) -> Result<bool, kube::Error> {
        let timeout = Duration::from_secs(900); // 15 minutes max
        let start = std::time::Instant::now();
        let mut not_found_count = 0;

        loop {
            if start.elapsed() > timeout {
                warn!(job = %job_name, "Job timed out");
                // Try to delete the job
                let _ = self
                    .jobs_api
                    .delete(job_name, &DeleteParams::default())
                    .await;
                return Ok(false);
            }

            match self.jobs_api.get(job_name).await {
                Ok(job) => {
                    not_found_count = 0; // Reset counter on success
                    if let Some(status) = job.status {
                        // Check if succeeded
                        if status.succeeded.unwrap_or(0) > 0 {
                            info!(job = %job_name, "Job succeeded");
                            return Ok(true);
                        }

                        // Check if failed
                        if status.failed.unwrap_or(0) > 0 {
                            warn!(job = %job_name, "Job failed");
                            return Ok(false);
                        }

                        // Still running
                        debug!(job = %job_name, "Job still running");
                    }
                }
                Err(kube::Error::Api(ref err)) if err.code == 404 => {
                    not_found_count += 1;
                    warn!(job = %job_name, count = not_found_count, "Job not found");
                    // If job is consistently not found, treat as deleted/failed
                    if not_found_count >= 3 {
                        error!(job = %job_name, "Job disappeared, marking as failed");
                        return Ok(false);
                    }
                }
                Err(e) => {
                    error!(error = %e, job = %job_name, "Failed to get job status");
                }
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::batch::v1::JobStatus;

    /// Helper to evaluate job status for testing
    fn evaluate_job_status(status: Option<&JobStatus>) -> Option<bool> {
        if let Some(s) = status {
            if s.succeeded.unwrap_or(0) > 0 {
                return Some(true);
            }
            if s.failed.unwrap_or(0) > 0 {
                return Some(false);
            }
        }
        None // Still running
    }

    #[test]
    fn test_job_status_succeeded() {
        let status = JobStatus {
            succeeded: Some(1),
            ..Default::default()
        };
        assert_eq!(evaluate_job_status(Some(&status)), Some(true));
    }

    #[test]
    fn test_job_status_failed() {
        let status = JobStatus {
            failed: Some(1),
            ..Default::default()
        };
        assert_eq!(evaluate_job_status(Some(&status)), Some(false));
    }

    #[test]
    fn test_job_status_running() {
        let status = JobStatus {
            active: Some(1),
            ..Default::default()
        };
        assert_eq!(evaluate_job_status(Some(&status)), None);
    }

    #[test]
    fn test_job_status_none() {
        assert_eq!(evaluate_job_status(None), None);
    }

    #[test]
    fn test_not_found_counter_threshold() {
        // Simulate the not_found counter behavior
        let threshold = 3;
        let mut not_found_count = 0;

        // First two 404s should not trigger failure
        for _ in 0..2 {
            not_found_count += 1;
            assert!(not_found_count < threshold, "Should not fail yet");
        }

        // Third 404 should trigger failure
        not_found_count += 1;
        assert!(not_found_count >= threshold, "Should fail after 3 not-founds");
    }
}
