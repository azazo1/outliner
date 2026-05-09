use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::model::{
    DebugTraceArtifactRecord, DebugTraceManifest, DebugTraceOutcomeRecord, DebugTraceStageRecord,
    RunOutcome,
};

#[derive(Debug, Clone, Default)]
pub struct DebugTraceRecorder {
    inner: Option<Arc<DebugTraceRecorderInner>>,
}

#[derive(Debug)]
struct DebugTraceRecorderInner {
    root: PathBuf,
    state: Mutex<DebugTraceState>,
}

#[derive(Debug)]
struct DebugTraceState {
    manifest: DebugTraceManifest,
    next_stage_index: usize,
}

impl DebugTraceRecorder {
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn new(root: PathBuf, input_path: &Path) -> Result<Self> {
        fs::create_dir_all(root.join("artifacts"))
            .with_context(|| format!("failed to create trace artifacts dir {}", root.display()))?;
        fs::create_dir_all(root.join("stages"))
            .with_context(|| format!("failed to create trace stages dir {}", root.display()))?;

        let recorder = Self {
            inner: Some(Arc::new(DebugTraceRecorderInner {
                root,
                state: Mutex::new(DebugTraceState {
                    manifest: DebugTraceManifest {
                        input_path: input_path.display().to_string(),
                        output_path: None,
                        stage_records: Vec::new(),
                        artifacts: Vec::new(),
                        final_outcome: None,
                    },
                    next_stage_index: 0,
                }),
            })),
        };
        recorder.write_manifest()?;
        Ok(recorder)
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn record_text_artifact(&self, kind: &str, content: &str) -> Result<Option<String>> {
        self.record_bytes_artifact(kind, "txt", content.as_bytes())
    }

    pub fn record_json_artifact<T>(&self, kind: &str, value: &T) -> Result<Option<String>>
    where
        T: Serialize,
    {
        let bytes = serde_json::to_vec_pretty(value)
            .with_context(|| format!("failed to serialize trace artifact {kind}"))?;
        self.record_bytes_artifact(kind, "json", &bytes)
    }

    pub fn record_binary_artifact(
        &self,
        kind: &str,
        extension: &str,
        bytes: &[u8],
    ) -> Result<Option<String>> {
        self.record_bytes_artifact(kind, extension, bytes)
    }

    pub fn record_stage(&self, record: DebugTraceStageRecord) -> Result<()> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };

        let stage_filename = {
            let mut state = inner
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("trace state lock poisoned"))?;
            state.next_stage_index += 1;
            let filename = format!(
                "{:04}_{}.json",
                state.next_stage_index,
                sanitize_stage_name(&record.stage_name)
            );
            state.manifest.stage_records.push(record.clone());
            filename
        };

        let stage_path = inner.root.join("stages").join(stage_filename);
        let stage_bytes = serde_json::to_vec_pretty(&record)
            .context("failed to serialize stage trace record")?;
        fs::write(&stage_path, stage_bytes)
            .with_context(|| format!("failed to write stage trace {}", stage_path.display()))?;
        self.write_manifest()
    }

    pub fn record_outcome(&self, outcome: &RunOutcome) -> Result<()> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };

        let mut state = inner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("trace state lock poisoned"))?;
        match outcome {
            RunOutcome::NoTocFound { reason, .. } => {
                state.manifest.final_outcome = Some(DebugTraceOutcomeRecord {
                    status: "no_toc_found".to_string(),
                    reason: Some(reason.clone()),
                    entries: None,
                    output_path: None,
                });
                state.manifest.output_path = None;
            }
            RunOutcome::AlreadyAligned { entries, .. } => {
                state.manifest.final_outcome = Some(DebugTraceOutcomeRecord {
                    status: "already_aligned".to_string(),
                    reason: None,
                    entries: Some(*entries),
                    output_path: None,
                });
                state.manifest.output_path = None;
            }
            RunOutcome::Updated {
                output_path,
                entries,
                ..
            } => {
                state.manifest.final_outcome = Some(DebugTraceOutcomeRecord {
                    status: "updated".to_string(),
                    reason: None,
                    entries: Some(*entries),
                    output_path: Some(output_path.display().to_string()),
                });
                state.manifest.output_path = Some(output_path.display().to_string());
            }
        }
        drop(state);
        self.write_manifest()
    }

    #[cfg(test)]
    pub fn root(&self) -> Option<&Path> {
        self.inner.as_ref().map(|inner| inner.root.as_path())
    }

    fn record_bytes_artifact(
        &self,
        kind: &str,
        extension: &str,
        bytes: &[u8],
    ) -> Result<Option<String>> {
        let Some(inner) = &self.inner else {
            return Ok(None);
        };

        let digest = Sha256::digest(bytes);
        let hash = format!("{digest:x}");
        let id = format!("sha256:{hash}.{extension}");
        let relative_path = format!("artifacts/{hash}.{extension}");
        let artifact_path = inner.root.join(&relative_path);

        if !artifact_path.exists() {
            fs::write(&artifact_path, bytes)
                .with_context(|| format!("failed to write trace artifact {}", artifact_path.display()))?;
        }

        let mut state = inner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("trace state lock poisoned"))?;
        if !state.manifest.artifacts.iter().any(|artifact| artifact.id == id) {
            state.manifest.artifacts.push(DebugTraceArtifactRecord {
                id: id.clone(),
                kind: kind.to_string(),
                relative_path,
            });
        }
        drop(state);

        self.write_manifest()?;
        Ok(Some(id))
    }

    fn write_manifest(&self) -> Result<()> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };

        let state = inner
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("trace state lock poisoned"))?;
        let manifest_path = inner.root.join("manifest.json");
        let bytes = serde_json::to_vec_pretty(&state.manifest)
            .context("failed to serialize trace manifest")?;
        fs::write(&manifest_path, bytes)
            .with_context(|| format!("failed to write trace manifest {}", manifest_path.display()))
    }
}

fn sanitize_stage_name(stage_name: &str) -> String {
    let mut sanitized = String::with_capacity(stage_name.len());
    for ch in stage_name.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch.to_ascii_lowercase());
        } else if matches!(ch, ' ' | '-' | '_') {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        "stage".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::DebugTraceRecorder;
    use crate::model::{DebugTraceStageRecord, DebugTraceUsageSnapshot, RunOutcome};
    use rig::completion::Usage;
    use std::path::PathBuf;

    #[test]
    fn repeated_artifacts_are_deduplicated() {
        let temp_dir = TempDir::new().expect("temp dir");
        let recorder = DebugTraceRecorder::new(
            temp_dir.path().join("trace"),
            PathBuf::from("book.pdf").as_path(),
        )
        .expect("trace recorder");

        let first = recorder
            .record_text_artifact("sample", "same content")
            .expect("first artifact");
        let second = recorder
            .record_text_artifact("sample", "same content")
            .expect("second artifact");

        assert_eq!(first, second);
    }

    #[test]
    fn manifest_records_stage_and_outcome() {
        let temp_dir = TempDir::new().expect("temp dir");
        let recorder = DebugTraceRecorder::new(
            temp_dir.path().join("trace"),
            PathBuf::from("book.pdf").as_path(),
        )
        .expect("trace recorder");

        recorder
            .record_stage(DebugTraceStageRecord {
                stage_name: "test stage".to_string(),
                page_range: Some("1..2".to_string()),
                worker: Some("worker 1/1".to_string()),
                artifact_refs: vec!["sha256:abc.txt".to_string()],
                usage: DebugTraceUsageSnapshot::from_usage(&Usage::new()),
                duration_ms: Some(10),
            })
            .expect("record stage");
        recorder
            .record_outcome(&RunOutcome::AlreadyAligned {
                entries: 3,
                usage: Usage::new(),
                agent_calls: 1,
            })
            .expect("record outcome");

        let manifest_path = recorder.root().expect("root").join("manifest.json");
        assert!(manifest_path.exists());
    }
}
