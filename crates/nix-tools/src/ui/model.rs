use std::collections::BTreeMap;

use nix_tools_engine::{
    DerivationNode, Manifest, ManifestOutcome, NodeState, Phase, ProgressEvent,
};

const PHASES: [Phase; 5] = [
    Phase::Discovery,
    Phase::Evaluation,
    Phase::Graph,
    Phase::Probe,
    Phase::Realization,
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PhaseStatus {
    #[default]
    Waiting,
    Active,
    Complete,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JobStatus {
    #[default]
    Queued,
    Running,
    Settled(NodeState),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    pub drv_path: String,
    pub label: String,
    pub dependencies: Vec<usize>,
    pub status: JobStatus,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub title: String,
    phases: BTreeMap<Phase, PhaseStatus>,
    jobs: Vec<Job>,
    job_index: BTreeMap<String, usize>,
    selected: Option<usize>,
    pub cancelled: Option<i32>,
    finished: bool,
    pub outcome: Option<ManifestOutcome>,
    help_visible: bool,
}

impl Model {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            phases: PHASES
                .into_iter()
                .map(|phase| (phase, PhaseStatus::Waiting))
                .collect(),
            jobs: Vec::new(),
            job_index: BTreeMap::new(),
            selected: None,
            cancelled: None,
            finished: false,
            outcome: None,
            help_visible: false,
        }
    }

    pub fn apply(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::PhaseStarted(phase) => {
                self.phases.insert(phase, PhaseStatus::Active);
            }
            ProgressEvent::PhaseFinished(phase) => {
                self.phases.insert(phase, PhaseStatus::Complete);
            }
            ProgressEvent::GraphDiscovered(nodes) => self.set_graph(nodes),
            ProgressEvent::NodeStarted { drv_path } => {
                self.set_job_status(&drv_path, JobStatus::Running);
            }
            ProgressEvent::NodeFinished { drv_path, state } => {
                self.set_job_status(&drv_path, JobStatus::Settled(state));
            }
            ProgressEvent::Cancelled { signal } => self.cancelled = Some(signal),
        }
    }

    pub fn finish(&mut self, manifest: &Manifest) {
        if self.jobs.is_empty() {
            self.set_graph(manifest.graph.clone());
        }
        let mut root_names = BTreeMap::<&str, Vec<&str>>::new();
        for root in &manifest.roots {
            if let Some(drv_path) = root.drv_path.as_deref() {
                root_names.entry(drv_path).or_default().push(&root.name);
            }
        }
        for job in &mut self.jobs {
            if let Some(names) = root_names.get(job.drv_path.as_str()) {
                job.label = names.join(" · ");
            }
        }
        for node in &manifest.nodes {
            self.set_job_status(&node.drv_path, JobStatus::Settled(node.state));
        }
        self.outcome = Some(manifest.outcome);
        self.finished = true;
    }

    pub fn complete(&mut self) {
        self.finished = true;
    }

    pub const fn finished(&self) -> bool {
        self.finished
    }

    pub const fn help_visible(&self) -> bool {
        self.help_visible
    }

    pub fn toggle_help(&mut self) {
        self.help_visible = !self.help_visible;
    }

    pub fn phase(&self, phase: Phase) -> PhaseStatus {
        self.phases.get(&phase).copied().unwrap_or_default()
    }

    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    pub const fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn select_next(&mut self) {
        self.selected = select(self.selected, self.jobs.len(), 1);
    }

    pub fn select_previous(&mut self) {
        self.selected = select(self.selected, self.jobs.len(), -1);
    }

    #[cfg(test)]
    pub fn focused_dependencies(&self) -> Vec<usize> {
        self.selected
            .and_then(|selected| self.jobs.get(selected))
            .map_or_else(Vec::new, |job| job.dependencies.clone())
    }

    fn set_job_status(&mut self, drv_path: &str, status: JobStatus) {
        if let Some(job) = self
            .job_index
            .get(drv_path)
            .and_then(|index| self.jobs.get_mut(*index))
        {
            job.status = status;
        }
    }

    fn set_graph(&mut self, nodes: Vec<DerivationNode>) {
        self.job_index = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.drv_path.clone(), index))
            .collect();
        self.jobs = nodes
            .into_iter()
            .map(|node| Job {
                label: derivation_label(&node.drv_path),
                dependencies: node
                    .dependencies
                    .keys()
                    .filter_map(|dependency| self.job_index.get(dependency).copied())
                    .collect(),
                drv_path: node.drv_path,
                status: JobStatus::Queued,
            })
            .collect();
        self.selected = (!self.jobs.is_empty()).then_some(0);
    }
}

fn select(current: Option<usize>, length: usize, delta: isize) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let signed_length = isize::try_from(length).ok()?;
    let delta = usize::try_from(delta.rem_euclid(signed_length)).ok()?;
    let current = current.unwrap_or_default() % length;
    let remaining = length - current;
    Some(if delta >= remaining {
        delta - remaining
    } else {
        current + delta
    })
}

fn derivation_label(drv_path: &str) -> String {
    let stem = drv_path
        .rsplit('/')
        .next()
        .unwrap_or(drv_path)
        .strip_suffix(".drv")
        .unwrap_or(drv_path);
    stem.split_once('-')
        .filter(|(hash, _)| {
            hash.len() == 32
                && hash
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .map_or(stem, |(_, label)| label)
        .to_owned()
}
