use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    AwaitingResult,
    Provisional(NodeState),
    Settled(NodeState),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JobFilter {
    #[default]
    All,
    Active,
    Waiting,
    Queued,
    Completed,
    Failed,
}

impl JobFilter {
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Waiting => "waiting",
            Self::Queued => "queued",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    const fn next(self, reverse: bool) -> Self {
        match (self, reverse) {
            (Self::All, false) | (Self::Waiting, true) => Self::Active,
            (Self::Active, false) | (Self::Queued, true) => Self::Waiting,
            (Self::Waiting, false) | (Self::Completed, true) => Self::Queued,
            (Self::Queued, false) | (Self::Failed, true) => Self::Completed,
            (Self::Completed, false) | (Self::All, true) => Self::Failed,
            (Self::Failed, false) | (Self::Active, true) => Self::All,
        }
    }

    const fn matches(self, status: JobStatus) -> bool {
        match self {
            Self::All => true,
            Self::Active => matches!(status, JobStatus::Running),
            Self::Waiting => {
                matches!(
                    status,
                    JobStatus::AwaitingResult | JobStatus::Provisional(_)
                )
            }
            Self::Queued => matches!(status, JobStatus::Queued),
            Self::Completed => matches!(status, JobStatus::Settled(_)),
            Self::Failed => matches!(status, JobStatus::Settled(NodeState::Failed)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    pub drv_path: String,
    pub label: String,
    pub dependencies: Vec<usize>,
    pub relationships_known: bool,
    pub dependents: Vec<usize>,
    pub status: JobStatus,
    pub started: Option<Duration>,
    pub settled: Option<Duration>,
    pub progress: Option<(u64, u64)>,
    pub logs: VecDeque<String>,
    pub log_scroll: usize,
}

impl Job {
    pub fn elapsed(&self, now: Duration) -> Option<Duration> {
        self.settled
            .or_else(|| self.started.map(|start| now.saturating_sub(start)))
    }
}

/// Where the model reads time from. Production measures the real thing; a test drives it, so no
/// assertion about what the interface renders depends on how long the test itself took.
#[derive(Clone, Copy, Debug)]
enum TimeSource {
    System(Instant),
    #[cfg(test)]
    Fixed(Duration),
}

impl TimeSource {
    fn now(self) -> Duration {
        match self {
            Self::System(start) => start.elapsed(),
            #[cfg(test)]
            Self::Fixed(elapsed) => elapsed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Model {
    pub title: String,
    phases: BTreeMap<Phase, PhaseStatus>,
    jobs: Vec<Job>,
    job_index: BTreeMap<String, usize>,
    visible_jobs: Vec<usize>,
    selected: Option<usize>,
    pub cancelled: Option<i32>,
    time: TimeSource,
    finished_at: Option<Duration>,
    finished: bool,
    pub outcome: Option<ManifestOutcome>,
    help_visible: bool,
    filter_query: String,
    filter_input_active: bool,
    job_filter: JobFilter,
}

impl Model {
    pub fn new(title: impl Into<String>) -> Self {
        Self::with_time(title, TimeSource::System(Instant::now()))
    }

    /// Builds a model whose time only moves when [`Model::advance`] says so.
    #[cfg(test)]
    pub fn fixed(title: impl Into<String>) -> Self {
        Self::with_time(title, TimeSource::Fixed(Duration::ZERO))
    }

    /// Moves a fixed model's clock forward.
    #[cfg(test)]
    pub fn advance(&mut self, step: Duration) {
        if let TimeSource::Fixed(elapsed) = &mut self.time {
            *elapsed += step;
        }
    }

    fn with_time(title: impl Into<String>, time: TimeSource) -> Self {
        Self {
            title: title.into(),
            phases: PHASES
                .into_iter()
                .map(|phase| (phase, PhaseStatus::Waiting))
                .collect(),
            jobs: Vec::new(),
            job_index: BTreeMap::new(),
            visible_jobs: Vec::new(),
            selected: None,
            cancelled: None,
            time,
            finished_at: None,
            finished: false,
            outcome: None,
            help_visible: false,
            filter_query: String::new(),
            filter_input_active: false,
            job_filter: JobFilter::All,
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
            ProgressEvent::GraphIncomplete => {}
            ProgressEvent::NodeStarted { drv_path } => {
                self.set_job_status(&drv_path, JobStatus::Running);
            }
            ProgressEvent::NodeActivityStopped { drv_path } => {
                self.set_job_status(&drv_path, JobStatus::AwaitingResult);
            }
            ProgressEvent::NodeProvisionalFinished { drv_path, state } => {
                self.set_job_status(&drv_path, JobStatus::Provisional(state));
            }
            ProgressEvent::NodeLogLine { drv_path, line } => {
                let index = self.ensure_job(&drv_path);
                if let Some(job) = self.jobs.get_mut(index) {
                    if job.logs.len() == 1_000 {
                        job.logs.pop_front();
                    }
                    job.logs.push_back(line);
                    if job.log_scroll > 0 {
                        job.log_scroll = (job.log_scroll + 1).min(job.logs.len().saturating_sub(1));
                    }
                }
            }
            ProgressEvent::NodeProgress {
                drv_path,
                done,
                expected,
            } => self.set_job_progress(&drv_path, done, expected),
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
            self.set_job_status_inner(&node.drv_path, JobStatus::Settled(node.state));
        }
        for job in &mut self.jobs {
            if manifest.outcome == nix_tools_engine::ManifestOutcome::Cancelled
                && !matches!(job.status, JobStatus::Settled(_))
            {
                job.status = JobStatus::Settled(NodeState::Cancelled);
            } else if let JobStatus::Provisional(state) = job.status {
                job.status = JobStatus::Settled(state);
            }
        }
        self.rebuild_visible_jobs();
        self.outcome = Some(manifest.outcome);
        self.complete();
    }

    pub fn complete(&mut self) {
        let now = self.time.now();
        self.finished_at.get_or_insert(now);
        self.finished = true;
    }

    /// Returns how long this model has been running, frozen once it finished.
    pub fn now(&self) -> Duration {
        self.finished_at.unwrap_or_else(|| self.time.now())
    }

    pub fn elapsed(&self) -> Duration {
        self.now()
    }

    pub fn settled(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| matches!(job.status, JobStatus::Settled(_)))
            .count()
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

    pub fn visible_job_indices(&self) -> &[usize] {
        &self.visible_jobs
    }

    pub fn selected_visible(&self) -> Option<usize> {
        let selected = self.selected?;
        self.visible_jobs.binary_search(&selected).ok()
    }

    pub const fn filter_query(&self) -> &str {
        self.filter_query.as_str()
    }

    pub const fn filter_input_active(&self) -> bool {
        self.filter_input_active
    }

    pub const fn job_filter(&self) -> JobFilter {
        self.job_filter
    }

    pub fn start_filter_input(&mut self) {
        self.filter_input_active = true;
    }

    pub fn finish_filter_input(&mut self) {
        self.filter_input_active = false;
    }

    pub fn push_filter_character(&mut self, character: char) {
        self.filter_query.push(character);
        self.rebuild_visible_jobs();
    }

    pub fn pop_filter_character(&mut self) {
        self.filter_query.pop();
        self.rebuild_visible_jobs();
    }

    pub fn clear_filters(&mut self) {
        self.filter_query.clear();
        self.filter_input_active = false;
        self.job_filter = JobFilter::All;
        self.rebuild_visible_jobs();
    }

    pub fn cycle_job_filter(&mut self, reverse: bool) {
        self.set_job_filter(self.job_filter.next(reverse));
    }

    pub fn set_job_filter(&mut self, filter: JobFilter) {
        self.job_filter = filter;
        self.rebuild_visible_jobs();
    }

    pub const fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn scroll_logs(&mut self, lines: isize) {
        if let Some(job) = self.selected.and_then(|index| self.jobs.get_mut(index)) {
            job.log_scroll = job
                .log_scroll
                .saturating_add_signed(lines)
                .min(job.logs.len().saturating_sub(1));
        }
    }

    pub fn follow_logs(&mut self) {
        if let Some(job) = self.selected.and_then(|index| self.jobs.get_mut(index)) {
            job.log_scroll = 0;
        }
    }

    pub fn select_next(&mut self) {
        self.select_visible(1);
    }

    pub fn select_previous(&mut self) {
        self.select_visible(-1);
    }

    pub fn select_first(&mut self) {
        self.selected = self.visible_jobs.first().copied();
    }

    pub fn select_last(&mut self) {
        self.selected = self.visible_jobs.last().copied();
    }

    #[cfg(test)]
    pub fn focused_dependencies(&self) -> Vec<usize> {
        self.selected
            .and_then(|selected| self.jobs.get(selected))
            .map_or_else(Vec::new, |job| job.dependencies.clone())
    }

    fn set_job_status(&mut self, drv_path: &str, status: JobStatus) {
        self.ensure_job(drv_path);
        if let Some(index) = self.set_job_status_inner(drv_path, status)
            && self.job_filter != JobFilter::All
        {
            self.reconcile_job_visibility(index);
        }
    }

    fn set_job_status_inner(&mut self, drv_path: &str, status: JobStatus) -> Option<usize> {
        let now = self.time.now();
        let index = self.job_index.get(drv_path).copied()?;
        if let Some(job) = self.jobs.get_mut(index) {
            match status {
                JobStatus::Running => {
                    if matches!(
                        job.status,
                        JobStatus::AwaitingResult | JobStatus::Provisional(_)
                    ) {
                        job.started = job
                            .settled
                            .take()
                            .map(|elapsed| now.saturating_sub(elapsed));
                        job.progress = None;
                    }
                    job.started.get_or_insert(now);
                }
                JobStatus::Settled(_) | JobStatus::AwaitingResult | JobStatus::Provisional(_) => {
                    if let Some(start) = job.started {
                        job.settled.get_or_insert(now.saturating_sub(start));
                    }
                }
                JobStatus::Queued => {}
            }
            job.status = status;
        }
        Some(index)
    }

    fn set_job_progress(&mut self, drv_path: &str, done: u64, expected: u64) {
        let index = self.ensure_job(drv_path);
        if let Some(job) = self.jobs.get_mut(index) {
            job.progress = Some((done, expected));
        }
    }

    fn set_graph(&mut self, nodes: Vec<Arc<DerivationNode>>) {
        let selected_path = self
            .selected
            .and_then(|index| self.jobs.get(index))
            .map(|job| job.drv_path.clone());
        let mut existing = std::mem::take(&mut self.jobs)
            .into_iter()
            .map(|job| (job.drv_path.clone(), job))
            .collect::<BTreeMap<_, _>>();
        self.job_index = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.drv_path.clone(), index))
            .collect();
        self.jobs = nodes
            .into_iter()
            .map(|node| {
                let dependencies = node
                    .dependencies
                    .keys()
                    .filter_map(|dependency| self.job_index.get(dependency).copied())
                    .collect();
                if let Some(mut job) = existing.remove(&node.drv_path) {
                    job.dependencies = dependencies;
                    job.relationships_known = true;
                    job.dependents.clear();
                    return job;
                }
                Job {
                    label: derivation_label(&node.drv_path),
                    dependencies,
                    relationships_known: true,
                    drv_path: node.drv_path.clone(),
                    dependents: Vec::new(),
                    status: JobStatus::Queued,
                    started: None,
                    settled: None,
                    progress: None,
                    logs: VecDeque::new(),
                    log_scroll: 0,
                }
            })
            .collect();
        for index in 0..self.jobs.len() {
            for dependency in self.jobs[index].dependencies.clone() {
                self.jobs[dependency].dependents.push(index);
            }
        }
        self.selected = selected_path
            .and_then(|path| self.job_index.get(&path).copied())
            .or_else(|| (!self.jobs.is_empty()).then_some(0));
        self.rebuild_visible_jobs();
    }

    fn ensure_job(&mut self, drv_path: &str) -> usize {
        if let Some(index) = self.job_index.get(drv_path) {
            return *index;
        }
        let index = self.jobs.len();
        self.job_index.insert(drv_path.to_owned(), index);
        self.jobs.push(Job {
            drv_path: drv_path.to_owned(),
            label: derivation_label(drv_path),
            dependencies: Vec::new(),
            relationships_known: false,
            dependents: Vec::new(),
            status: JobStatus::Queued,
            started: None,
            settled: None,
            progress: None,
            logs: VecDeque::new(),
            log_scroll: 0,
        });
        self.reconcile_job_visibility(index);
        index
    }

    fn select_visible(&mut self, delta: isize) {
        let current = self
            .selected
            .and_then(|selected| self.visible_jobs.binary_search(&selected).ok());
        self.selected = select(current, self.visible_jobs.len(), delta)
            .map(|position| self.visible_jobs[position]);
    }

    fn rebuild_visible_jobs(&mut self) {
        let query = self.filter_query.to_lowercase();
        let visible_jobs = (0..self.jobs.len())
            .filter(|index| self.job_matches_query(*index, &query))
            .collect();
        self.visible_jobs = visible_jobs;
        if self
            .selected
            .is_none_or(|selected| self.visible_jobs.binary_search(&selected).is_err())
        {
            self.selected = self.visible_jobs.first().copied();
        }
    }

    fn reconcile_job_visibility(&mut self, index: usize) {
        match (
            self.visible_jobs.binary_search(&index),
            self.job_matches(index),
        ) {
            (Err(position), true) => self.visible_jobs.insert(position, index),
            (Ok(position), false) => {
                self.visible_jobs.remove(position);
                if self.selected == Some(index) {
                    self.selected = self
                        .visible_jobs
                        .get(position)
                        .or_else(|| self.visible_jobs.last())
                        .copied();
                }
            }
            _ => {}
        }
        if self.selected.is_none() && self.job_matches(index) {
            self.selected = Some(index);
        }
    }

    fn job_matches(&self, index: usize) -> bool {
        self.job_matches_query(index, &self.filter_query.to_lowercase())
    }

    fn job_matches_query(&self, index: usize, query: &str) -> bool {
        let Some(job) = self.jobs.get(index) else {
            return false;
        };
        if !self.job_filter.matches(job.status) {
            return false;
        }
        query.is_empty()
            || job.label.to_lowercase().contains(query)
            || job.drv_path.to_lowercase().contains(query)
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
