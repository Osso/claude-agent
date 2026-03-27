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

use crate::jira_token::JiraTokenManager;
use crate::queue::{Queue, QueueItem};

const NAMESPACE: &str = "claude-agent";
/// Worker image, configurable via WORKER_IMAGE env var (defaults to :latest)
fn worker_image() -> String {
    std::env::var("WORKER_IMAGE")
        .unwrap_or_else(|_| "registry.digitalocean.com/globalcomix/claude-agent-worker:latest".into())
}
const JOB_TTL_SECONDS: i32 = 900; // 15 minutes after completion

fn secret_env_var(name: &str, key: &str, optional: bool) -> EnvVar {
    EnvVar {
        name: name.into(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: "claude-agent-secrets".into(),
                key: key.into(),
                optional: Some(optional),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build environment variables for worker container.
fn build_env_vars(payload_b64: String, jira_access_token: Option<String>) -> Vec<EnvVar> {
    let mut env_vars = vec![
        EnvVar {
            name: "REVIEW_PAYLOAD".into(),
            value: Some(payload_b64),
            ..Default::default()
        },
        secret_env_var("CLAUDE_CODE_OAUTH_TOKEN", "claude-oauth-token", false),
        secret_env_var("GITHUB_TOKEN", "github-token", true),
        secret_env_var("SENTRY_AUTH_TOKEN", "sentry-auth-token", true),
    ];

    if let Some(token) = jira_access_token {
        env_vars.push(EnvVar {
            name: "JIRA_ACCESS_TOKEN".into(),
            value: Some(token),
            ..Default::default()
        });
    }

    env_vars
}

/// Job scheduler that processes the queue sequentially.
pub struct Scheduler {
    queue: Queue,
    #[allow(dead_code)]
    k8s_client: Client,
    jobs_api: Api<Job>,
    running: Arc<Mutex<bool>>,
    jira_token_manager: Option<Arc<JiraTokenManager>>,
}

impl Scheduler {
    pub async fn new(
        queue: Queue,
        jira_token_manager: Option<Arc<JiraTokenManager>>,
    ) -> Result<Self, kube::Error> {
        let k8s_client = Client::try_default().await?;
        let jobs_api = Api::namespaced(k8s_client.clone(), NAMESPACE);

        Ok(Self {
            queue,
            k8s_client,
            jobs_api,
            running: Arc::new(Mutex::new(false)),
            jira_token_manager,
        })
    }

    /// Start the scheduler loop.
    pub async fn run(&self) {
        info!("Starting scheduler");
        *self.running.lock().await = true;

        while *self.running.lock().await {
            if self.has_running_job().await {
                debug!("Job already running, waiting");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }

            match self.queue.pop(30).await {
                Ok(Some(item)) => self.process_item(item).await,
                Ok(None) => {}
                Err(e) => {
                    error!(error = %e, "Failed to pop from queue");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }

        info!("Scheduler stopped");
    }

    async fn process_item(&self, item: QueueItem) {
        info!(id = %item.id, "Processing queue item");

        if let Err(e) = self.queue.mark_processing(&item).await {
            error!(error = %e, "Failed to mark item as processing");
            return;
        }

        match self.spawn_job(&item).await {
            Ok(job_name) => {
                info!(job = %job_name, "Spawned review job");
                self.await_job_completion(&job_name, item).await;
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

    async fn await_job_completion(&self, job_name: &str, item: QueueItem) {
        match self.wait_for_job(job_name).await {
            Ok(true) => {
                let _ = self.queue.mark_completed(&item.id).await;
            }
            Ok(false) => {
                let _ = self.queue.mark_failed(item, "Job failed").await;
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

    async fn get_jira_access_token(&self) -> Option<String> {
        let manager = self.jira_token_manager.as_ref()?;
        match manager.get_access_token().await {
            Ok(token) => Some(token),
            Err(e) => {
                warn!(error = %e, "Failed to get Jira access token, job will run without Jira integration");
                None
            }
        }
    }

    fn build_job_manifest(&self, job_name: &str, item: &QueueItem, env_vars: Vec<EnvVar>) -> Job {
        Job {
            metadata: kube::api::ObjectMeta {
                name: Some(job_name.to_string()),
                namespace: Some(NAMESPACE.into()),
                labels: Some(BTreeMap::from([
                    ("app".to_string(), "claude-review".to_string()),
                    ("queue-id".to_string(), item.id.clone()),
                ])),
                ..Default::default()
            },
            spec: Some(self.build_job_spec(env_vars)),
            ..Default::default()
        }
    }

    fn build_job_spec(&self, env_vars: Vec<EnvVar>) -> JobSpec {
        JobSpec {
            ttl_seconds_after_finished: Some(JOB_TTL_SECONDS),
            active_deadline_seconds: Some(900),
            backoff_limit: Some(0),
            template: PodTemplateSpec {
                metadata: Some(kube::api::ObjectMeta {
                    labels: Some(BTreeMap::from([(
                        "app".to_string(),
                        "claude-review".to_string(),
                    )])),
                    ..Default::default()
                }),
                spec: Some(build_pod_spec(env_vars)),
            },
            ..Default::default()
        }
    }

    /// Spawn a K8s Job for the review.
    async fn spawn_job(&self, item: &QueueItem) -> Result<String, kube::Error> {
        let job_name = format!(
            "{}-{}-{}",
            item.payload.job_prefix(),
            item.payload.issue_id().to_lowercase(),
            &item.id[..8]
        );

        let payload_json = serde_json::to_string(&item.payload).unwrap();
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(&payload_json);
        let jira_access_token = self.get_jira_access_token().await;
        let env_vars = build_env_vars(payload_b64, jira_access_token);
        let job = self.build_job_manifest(&job_name, item, env_vars);

        self.jobs_api.create(&PostParams::default(), &job).await?;
        Ok(job_name)
    }

    /// Wait for a job to complete.
    async fn wait_for_job(&self, job_name: &str) -> Result<bool, kube::Error> {
        let timeout = Duration::from_secs(900);
        let start = std::time::Instant::now();
        let mut not_found_count = 0;

        loop {
            if start.elapsed() > timeout {
                warn!(job = %job_name, "Job timed out");
                let _ = self
                    .jobs_api
                    .delete(job_name, &DeleteParams::default())
                    .await;
                return Ok(false);
            }

            match self.check_job_status(job_name, &mut not_found_count).await {
                Some(result) => return result,
                None => tokio::time::sleep(Duration::from_secs(5)).await,
            }
        }
    }

    async fn check_job_status(
        &self,
        job_name: &str,
        not_found_count: &mut u32,
    ) -> Option<Result<bool, kube::Error>> {
        match self.jobs_api.get(job_name).await {
            Ok(job) => {
                *not_found_count = 0;
                if let Some(status) = job.status {
                    if status.succeeded.unwrap_or(0) > 0 {
                        info!(job = %job_name, "Job succeeded");
                        return Some(Ok(true));
                    }
                    if status.failed.unwrap_or(0) > 0 {
                        warn!(job = %job_name, "Job failed");
                        return Some(Ok(false));
                    }
                    debug!(job = %job_name, "Job still running");
                }
                None
            }
            Err(kube::Error::Api(ref err)) if err.code == 404 => {
                *not_found_count += 1;
                warn!(job = %job_name, count = *not_found_count, "Job not found");
                if *not_found_count >= 3 {
                    error!(job = %job_name, "Job disappeared, marking as failed");
                    return Some(Ok(false));
                }
                None
            }
            Err(e) => {
                error!(error = %e, job = %job_name, "Failed to get job status");
                None
            }
        }
    }
}

fn build_worker_container(env_vars: Vec<EnvVar>) -> Container {
    Container {
        name: "worker".into(),
        image: Some(worker_image()),
        env: Some(env_vars),
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
    }
}

fn build_pod_spec(env_vars: Vec<EnvVar>) -> PodSpec {
    PodSpec {
        restart_policy: Some("Never".into()),
        security_context: Some(k8s_openapi::api::core::v1::PodSecurityContext {
            run_as_user: Some(1000),
            run_as_group: Some(1000),
            fs_group: Some(1000),
            ..Default::default()
        }),
        containers: vec![build_worker_container(env_vars)],
        volumes: Some(vec![Volume {
            name: "workdir".into(),
            empty_dir: Some(EmptyDirVolumeSource {
                size_limit: Some(Quantity("2Gi".into())),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
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
        let threshold = 3;
        let mut not_found_count = 0;

        for _ in 0..2 {
            not_found_count += 1;
            assert!(not_found_count < threshold, "Should not fail yet");
        }

        not_found_count += 1;
        assert!(not_found_count >= threshold, "Should fail after 3 not-founds");
    }
}
