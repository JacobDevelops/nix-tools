package nixtools

import "encoding/json"

const ProtocolVersion = 1

type Presentation struct {
	Mode  string `json:"mode"`
	Title string `json:"title"`
}

type Flake struct {
	Reference        string `json:"reference"`
	WorkingDirectory string `json:"working_directory,omitempty"`
}
type TrustedSubstituter struct {
	URL        string   `json:"url"`
	PublicKeys []string `json:"public_keys"`
}
type ResourceLimits struct {
	MaxJobs                  *uint64 `json:"max_jobs,omitempty"`
	EvaluationBatchSize      uint64  `json:"evaluation_batch_size,omitempty"`
	EvaluationConcurrency    uint64  `json:"evaluation_concurrency,omitempty"`
	SubstitutionConcurrency  uint64  `json:"substitution_concurrency,omitempty"`
	MaxProcessOutputBytes    uint64  `json:"max_process_output_bytes,omitempty"`
	MaxEvaluationMemoryBytes uint64  `json:"max_evaluation_memory_bytes,omitempty"`
	MaxRoots                 uint64  `json:"max_roots,omitempty"`
	MaxGraphNodes            uint64  `json:"max_graph_nodes,omitempty"`
	MaxGraphRetainedBytes    uint64  `json:"max_graph_retained_bytes,omitempty"`
	MaxGraphStreamBytes      uint64  `json:"max_graph_stream_bytes,omitempty"`
	MaxDiagnosticBytes       uint64  `json:"max_diagnostic_bytes,omitempty"`
}
type EngineConfig struct {
	NixExecutable       string               `json:"nix_executable,omitempty"`
	System              string               `json:"system"`
	TrustedSubstituters []TrustedSubstituter `json:"trusted_substituters,omitempty"`
	GraphMode           string               `json:"graph_mode,omitempty"`
	Limits              ResourceLimits       `json:"limits"`
}
type BuildRequest struct {
	AllOutputs bool
	Flake      Flake
	Targets    []string
	OutLink    string
	Rebuild    bool
	SkipCached bool
}

type BuildInstallablesRequest struct {
	AllOutputs     bool
	Flake          Flake
	AttributePaths [][]string
	OutLink        string
	Rebuild        bool
	SkipCached     bool
}
type CheckRequest struct {
	AllOutputs bool
	Flake      Flake
	Targets    []string
	OutLink    string
	Rebuild    bool
	SkipCached bool
}
type RunRequest struct {
	Flake   Flake
	App     string
	Rebuild bool
}
type Discovery struct {
	Packages []string `json:"packages"`
	Checks   []string `json:"checks"`
	Apps     []string `json:"apps"`
}
type PreparedRun struct {
	Program  string   `json:"program"`
	Manifest Manifest `json:"manifest"`
}
type DerivationNode struct {
	DrvPath      string              `json:"drv_path"`
	Dependencies map[string][]string `json:"dependencies"`
	Outputs      map[string]*string  `json:"outputs"`
}
type RootResult struct {
	Kind    string            `json:"kind"`
	Name    string            `json:"name"`
	DrvPath *string           `json:"drv_path"`
	Outputs map[string]string `json:"outputs"`
	State   string            `json:"state"`
}
type DependencyFailure struct {
	Dependency string `json:"dependency"`
}
type NodeResult struct {
	DrvPath           string             `json:"drv_path"`
	Dependencies      []string           `json:"dependencies"`
	RequiredOutputs   []string           `json:"required_outputs"`
	ProducedPaths     []string           `json:"produced_paths"`
	State             string             `json:"state"`
	DependencyFailure *DependencyFailure `json:"dependency_failure"`
}
type Availability struct {
	Path          string  `json:"path"`
	State         string  `json:"state"`
	Substituter   *string `json:"substituter"`
	NARBytes      *uint64 `json:"nar_bytes"`
	DownloadBytes *uint64 `json:"download_bytes"`
}
type Diagnostic struct {
	Phase     string  `json:"phase"`
	Code      string  `json:"code"`
	Severity  string  `json:"severity"`
	Target    *string `json:"target"`
	Message   string  `json:"message"`
	Stdout    string  `json:"stdout"`
	Stderr    string  `json:"stderr"`
	Truncated bool    `json:"truncated"`
}
type PhaseMetrics struct {
	Processes  uint64 `json:"processes"`
	DurationMS uint64 `json:"duration_ms"`
}
type NodeMetrics struct {
	DrvPath    string `json:"drv_path"`
	DurationMS uint64 `json:"duration_ms"`
}
type ManifestMetrics struct {
	Validation   PhaseMetrics  `json:"validation"`
	StartedAtMS  uint64        `json:"started_at_ms"`
	FinishedAtMS uint64        `json:"finished_at_ms"`
	Evaluation   PhaseMetrics  `json:"evaluation"`
	Graph        PhaseMetrics  `json:"graph"`
	Probe        PhaseMetrics  `json:"probe"`
	Realization  PhaseMetrics  `json:"realization"`
	Nodes        []NodeMetrics `json:"nodes"`
}
type Manifest struct {
	Schema       string           `json:"schema"`
	System       string           `json:"system"`
	Roots        []RootResult     `json:"roots"`
	Graph        []DerivationNode `json:"graph"`
	Availability []Availability   `json:"availability"`
	Nodes        []NodeResult     `json:"nodes"`
	Diagnostics  []Diagnostic     `json:"diagnostics"`
	Metrics      ManifestMetrics  `json:"metrics"`
	Outcome      string           `json:"outcome"`
}

// Unknown event kinds retain their raw data so newer engines remain usable.
type Event struct {
	Kind string          `json:"kind"`
	Data json.RawMessage `json:"data,omitempty"`
}
type NodeEvent struct {
	DrvPath  string `json:"drv_path"`
	State    string `json:"state,omitempty"`
	Line     string `json:"line,omitempty"`
	Done     uint64 `json:"done,omitempty"`
	Expected uint64 `json:"expected,omitempty"`
}
type Error struct {
	Signal   *int      `json:"signal,omitempty"`
	Category string    `json:"category,omitempty"`
	Code     string    `json:"code"`
	Message  string    `json:"message"`
	Manifest *Manifest `json:"manifest,omitempty"`
	Status   int       `json:"exit_code,omitempty"`
	Cause    error     `json:"-"`
}

func (e *Error) Error() string {
	return e.Message
}
func (e *Error) Unwrap() error { return e.Cause }
func (e *Error) ExitCode() int {
	if e.Status > 0 {
		return e.Status
	}
	return 1
}
