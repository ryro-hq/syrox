use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, mpsc::SyncSender};

use syrox_engine::{
    CheckConfiguration, ProjectAnalysis, open_project_analysis_with,
    open_standard_library_analysis_with,
};
use syrox_lang::{AnalysisCancellation, AnalysisSnapshot, SemanticAnalysis, SourceId};

use super::{Event, file_uri};

#[derive(Debug)]
pub(super) struct Job {
    pub mode: WorkspaceMode,
    pub generation: u64,
    pub reload: u64,
    pub root: PathBuf,
    pub documents: AnalysisSnapshot,
    pub cancellation: AnalysisCancellation,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) enum WorkspaceMode {
    #[default]
    Project,
    StandardLibrary,
}

#[derive(Debug)]
pub(super) struct ResultSet {
    pub generation: u64,
    pub analysis: Result<Arc<SemanticAnalysis>, String>,
    pub paths: BTreeMap<SourceId, String>,
}

impl ResultSet {
    pub(super) fn uri(&self, id: SourceId) -> String {
        self.paths.get(&id).cloned().unwrap_or_else(|| {
            format!(
                "syrox-source://{}/{}/{}.srx",
                std::process::id(),
                self.generation,
                id.index()
            )
        })
    }

    pub(super) fn virtual_source(&self, uri: &str) -> Option<&syrox_lang::Source> {
        if !uri.starts_with("syrox-source:") {
            return None;
        }
        self.analysis
            .as_ref()
            .ok()?
            .sources()
            .iter()
            .find(|(id, _)| !self.paths.contains_key(id) && self.uri(*id) == uri)
            .map(|(_, source)| source)
    }
}

/// One active job and one replaceable pending job; rapid edits cannot queue an
/// unbounded history of snapshots. The coordinator owns cancellation/publication.
#[derive(Debug, Default)]
pub(super) struct Jobs {
    pending: Mutex<(Option<Job>, bool)>,
    ready: Condvar,
}

impl Jobs {
    pub fn submit(&self, job: Job) {
        self.pending.lock().expect("job lock").0 = Some(job);
        self.ready.notify_one();
    }
    pub fn stop(&self) {
        *self.pending.lock().expect("job lock") = (None, true);
        self.ready.notify_one();
    }
    fn next(&self) -> Option<Job> {
        let mut pending = self.pending.lock().expect("job lock");
        loop {
            if pending.1 {
                return None;
            }
            if let Some(job) = pending.0.take() {
                return Some(job);
            }
            pending = self.ready.wait(pending).expect("job lock");
        }
    }
}

#[allow(clippy::too_many_lines)] // Single worker lifecycle, including snapshot publication.
pub(super) fn spawn(jobs: Arc<Jobs>, events: SyncSender<Event>, configuration: CheckConfiguration) {
    std::thread::spawn(move || {
        let mut project: Option<ProjectAnalysis> = None;
        let mut reload = None;
        let mut active = BTreeMap::<SourceId, i32>::new();
        let mut load_error = String::new();
        while let Some(job) = jobs.next() {
            if job.cancellation.check().is_err() {
                continue;
            }
            if reload != Some(job.reload) {
                let loaded = match job.mode {
                    WorkspaceMode::Project => open_project_analysis_with(&job.root, &configuration),
                    WorkspaceMode::StandardLibrary => {
                        open_standard_library_analysis_with(&job.root, &configuration)
                    }
                };
                project = match loaded {
                    Ok(project) => Some(project),
                    Err(error) => {
                        load_error = error.to_string();
                        None
                    }
                };
                active.clear();
                reload = Some(job.reload);
            }
            if job.cancellation.check().is_err() {
                continue;
            }
            let mut paths = BTreeMap::new();
            let analysis = if let Some(project) = project.as_mut() {
                paths.extend(
                    project
                        .snapshot()
                        .source_paths()
                        .iter()
                        .filter_map(|(id, path)| file_uri(path).map(|uri| (*id, uri))),
                );
                let mut present = BTreeSet::new();
                let mut update_error = None;
                for (id, uri) in &paths {
                    if let Some(document) = job.documents.document(uri) {
                        let version = document.overlay_version().expect("open buffer version");
                        present.insert(*id);
                        if active.get(id) != Some(&version) {
                            // Preserve content caches on ordinary newer versions;
                            // only a reopened sequence needs its old overlay closed.
                            let reset = if active.get(id).is_some_and(|old| version <= *old) {
                                project.close_overlay(*id).map(|_| ())
                            } else {
                                Ok(())
                            };
                            let result = reset.and_then(|()| {
                                project.set_overlay(*id, version, document.source().text())
                            });
                            match result {
                                Ok(_) => {
                                    active.insert(*id, version);
                                }
                                Err(error) => {
                                    update_error = Some(error.to_string());
                                }
                            }
                        }
                    }
                }
                for id in active
                    .keys()
                    .copied()
                    .filter(|id| !present.contains(id))
                    .collect::<Vec<_>>()
                {
                    if let Err(error) = project.close_overlay(id) {
                        update_error = Some(error.to_string());
                    }
                    active.remove(&id);
                }
                if let Some(error) = update_error {
                    Err(error)
                } else {
                    project
                        .snapshot()
                        .analyze(&job.cancellation)
                        .map_err(|error| error.to_string())
                }
            } else {
                Err(load_error.clone())
            };
            let analysis = analysis.and_then(|analysis| {
                analysis
                    .occurrences(&job.cancellation)
                    .map_err(|error| error.to_string())?;
                Ok(analysis)
            });
            if job.cancellation.check().is_ok()
                && events
                    .send(Event::Analyzed(ResultSet {
                        generation: job.generation,
                        analysis,
                        paths,
                    }))
                    .is_err()
            {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_work_is_coalesced_to_the_latest_snapshot() {
        let jobs = Jobs::default();
        let host = syrox_lang::AnalysisHost::default();
        for generation in 0..100 {
            jobs.submit(Job {
                mode: WorkspaceMode::Project,
                generation,
                reload: 0,
                root: PathBuf::new(),
                documents: host.snapshot(),
                cancellation: AnalysisCancellation::default(),
            });
        }
        assert_eq!(jobs.next().unwrap().generation, 99);
        jobs.stop();
        assert!(jobs.next().is_none());
    }
}
