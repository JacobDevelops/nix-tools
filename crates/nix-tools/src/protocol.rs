//! Versioned, single-operation JSON transport for non-Rust clients.

use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ui::{OutputMode, UiSession};
use nix_tools_core::outcome::{Error, ErrorKind};
use nix_tools_core::process::{Cancellation, ProcessRunner, StdProcessRunner};
use nix_tools_core::redaction::Redactor;
use nix_tools_engine::{
    BuildRequest, CheckRequest, DiscoverRequest, EngineConfig, EngineDependencies, EngineError,
    FlakeEngine, FlakeRef, GraphMode, NixEngine, ProgressEvent, ProgressSink, ResourceLimits,
    RunRequest, SystemClock, TrustedSubstituter,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const DEFAULT_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

fn default_response_bytes() -> usize {
    DEFAULT_RESPONSE_BYTES
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ResultPayload {
    Discover {
        discovery: nix_tools_engine::DiscoveredTargets,
    },
    Build {
        manifest: nix_tools_engine::Manifest,
    },
    BuildInstallables {
        manifest: nix_tools_engine::Manifest,
    },
    Check {
        manifest: nix_tools_engine::Manifest,
    },
    PrepareRun {
        program: String,
        manifest: nix_tools_engine::Manifest,
    },
    FlakeCheck {
        exit_code: u8,
        manifest: nix_tools_engine::Manifest,
    },
}

#[derive(Serialize)]
struct ResultMessage<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    version: u32,
    id: &'a str,
    result: &'a ResultPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<Value>,
}

#[derive(Serialize)]
struct ProgressMessage<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    version: u32,
    id: &'a str,
    event: &'a ProgressEvent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(rename = "type")]
    message_type: String,
    version: u32,
    id: String,
    operation: Operation,
    config: Config,
    flake: Flake,
    #[serde(default)]
    targets: Vec<String>,
    out_link: Option<PathBuf>,
    app: Option<String>,
    #[serde(default)]
    rebuild: bool,
    #[serde(default = "default_response_bytes")]
    max_response_bytes: usize,
    presentation: Option<Presentation>,
    #[serde(default)]
    attribute_paths: Vec<Vec<String>>,
    #[serde(default)]
    skip_cached: bool,
    #[serde(default)]
    all_outputs: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Presentation {
    mode: OutputMode,
    title: String,
}

impl ResultPayload {
    const fn manifest(&self) -> Option<&nix_tools_engine::Manifest> {
        match self {
            Self::Discover { .. } => None,
            Self::Build { manifest }
            | Self::BuildInstallables { manifest }
            | Self::Check { manifest }
            | Self::PrepareRun { manifest, .. }
            | Self::FlakeCheck { manifest, .. } => Some(manifest),
        }
    }

    fn failure(&self, cancellation: &Cancellation) -> Option<Value> {
        let operation = match self {
            Self::Discover { .. } => "discover",
            Self::Build { .. } | Self::BuildInstallables { .. } => "build",
            Self::Check { .. } => "check",
            Self::PrepareRun { .. } => "run",
            Self::FlakeCheck { .. } => "flake check",
        };
        self.manifest()
            .and_then(|manifest| crate::manifest_result(manifest, operation, cancellation).err())
            .map(|error| error_payload(&error, cancellation.signal()))
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Discover,
    Build,
    BuildInstallables,
    Check,
    PrepareRun,
    FlakeCheck,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_nix")]
    nix_executable: String,
    system: String,
    #[serde(default)]
    trusted_substituters: Vec<TrustedSubstituter>,
    #[serde(default)]
    graph_mode: GraphMode,
    #[serde(default)]
    limits: ResourceLimits,
}

fn default_nix() -> String {
    "nix".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Flake {
    reference: String,
    working_directory: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    #[serde(rename = "type")]
    message_type: String,
    version: u32,
    id: String,
    signal: i32,
}

impl Request {
    fn validate(&self) -> Result<(), Error> {
        if !(1024..=1024 * 1024 * 1024).contains(&self.max_response_bytes) {
            return Err(Error::usage(
                "max_response_bytes must be between 1024 and 1073741824",
            ));
        }
        if self.version != VERSION || self.message_type != "request" {
            return Err(Error::usage("unsupported protocol version or message type"));
        }
        if self.id.is_empty()
            || self.id.len() > 128
            || self.flake.reference.is_empty()
            || self.flake.reference.starts_with('-')
        {
            return Err(Error::usage(
                "request needs a nonempty id and a non-option flake reference",
            ));
        }
        if !matches!(
            self.operation,
            Operation::Build | Operation::BuildInstallables | Operation::Check
        ) && self.out_link.is_some()
            || !matches!(self.operation, Operation::Build | Operation::Check)
                && !self.targets.is_empty()
            || (self.operation == Operation::PrepareRun) != self.app.is_some()
            || self.app.as_ref().is_some_and(String::is_empty)
            || self.operation == Operation::Discover && self.rebuild
            || self.operation != Operation::BuildInstallables && !self.attribute_paths.is_empty()
            || !matches!(
                self.operation,
                Operation::Build | Operation::BuildInstallables | Operation::Check
            ) && (self.skip_cached || self.all_outputs)
        {
            return Err(Error::usage(
                "fields do not apply to the requested operation",
            ));
        }
        Ok(())
    }

    fn engine_config(&self) -> Result<EngineConfig, Error> {
        let mut config =
            EngineConfig::new(&self.config.nix_executable, self.config.system.parse()?);
        config
            .trusted_substituters
            .clone_from(&self.config.trusted_substituters);
        config.graph_mode = self.config.graph_mode;
        config.limits = self.config.limits;
        config.rebuild = self.rebuild;
        config.skip_cached = self.skip_cached;
        config.all_outputs = self.all_outputs;
        Ok(config)
    }

    fn engine_request(&self) -> nix_tools_engine::EngineRequest {
        use nix_tools_engine::EngineRequest;
        match self.operation {
            Operation::Discover => EngineRequest::Discover(DiscoverRequest {
                flake: self.flake_ref(),
            }),
            Operation::Build => EngineRequest::Build(BuildRequest {
                flake: self.flake_ref(),
                targets: self.targets.clone(),
                out_link: self.out_link.clone(),
            }),
            Operation::BuildInstallables => {
                EngineRequest::BuildInstallables(nix_tools_engine::BuildInstallablesRequest {
                    flake: self.flake_ref(),
                    attribute_paths: self.attribute_paths.clone(),
                    out_link: self.out_link.clone(),
                })
            }
            Operation::Check => EngineRequest::Check(CheckRequest {
                flake: self.flake_ref(),
                targets: self.targets.clone(),
                out_link: self.out_link.clone(),
            }),
            Operation::FlakeCheck => {
                EngineRequest::FlakeCheck(nix_tools_engine::FlakeCheckRequest {
                    flake: self.flake_ref(),
                })
            }
            Operation::PrepareRun => EngineRequest::Run(RunRequest {
                flake: self.flake_ref(),
                app: self.app.clone().unwrap_or_default(),
                arguments: Vec::new(),
            }),
        }
    }

    fn flake_ref(&self) -> FlakeRef {
        FlakeRef::new(&self.flake.reference, self.flake.working_directory.clone())
    }
}

struct Output {
    writer: Mutex<io::Stdout>,
    cancellation: Cancellation,
    id: String,
    max_response_bytes: usize,
    failure: Mutex<Option<Error>>,
    ui: Mutex<Option<UiSession>>,
}

struct ResponseBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for ResponseBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("response exceeds max_response_bytes"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_response(message: &impl Serialize, limit: usize) -> Result<Vec<u8>, Error> {
    let mut buffer = ResponseBuffer {
        bytes: Vec::new(),
        limit: limit.saturating_sub(1),
    };
    serde_json::to_writer(&mut buffer, message)
        .map_err(|error| Error::usage(format!("encode protocol response: {error}")))?;
    buffer.bytes.push(b'\n');
    Ok(buffer.bytes)
}

impl Output {
    fn send(&self, message: &impl Serialize) -> Result<(), Error> {
        let frame = encode_response(message, self.max_response_bytes)?;
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = writer.write_all(&frame).and_then(|()| writer.flush());
        result.map_err(|error| {
            self.cancellation.request(15);
            Error::io(format!("write protocol: {error}"))
        })
    }

    fn progress(&self, message: &impl Serialize) {
        if let Err(error) = self.send(message) {
            *self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
            self.cancellation.request(15);
        }
    }

    fn terminal(&self, result: Result<ResultPayload, Error>) -> Result<(), Error> {
        if let Some(mut ui) = self
            .ui
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            ui.finish(result.as_ref().ok().and_then(ResultPayload::manifest));
        }
        let failure = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let result = failure.map_or(result, Err).and_then(|result| {
            self.send(&ResultMessage {
                message_type: "result",
                version: VERSION,
                id: &self.id,
                result: &result,
                signal: self.cancellation.signal(),
                failure: result.failure(&self.cancellation),
            })
        });
        match result {
            Ok(()) => Ok(()),
            Err(error) => self
                .send(&error_envelope(
                    &self.id,
                    &error,
                    self.cancellation.signal(),
                ))
                .or_else(|error| {
                    self.send(&error_envelope(
                        &self.id,
                        &error,
                        self.cancellation.signal(),
                    ))
                }),
        }
    }
}

impl ProgressSink for Output {
    fn emit(&self, event: ProgressEvent) {
        if let Some(ui) = self
            .ui
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            ui.progress().emit(event.clone());
        }
        self.progress(&ProgressMessage {
            message_type: "progress",
            version: VERSION,
            id: &self.id,
            event: &event,
        });
    }
}

fn read_frame(reader: &mut impl BufRead) -> Result<Vec<u8>, Error> {
    let mut frame = Vec::new();
    reader
        .take(u64::try_from(MAX_REQUEST_BYTES).unwrap_or(u64::MAX) + 1)
        .read_until(b'\n', &mut frame)
        .map_err(|error| Error::io(format!("read protocol: {error}")))?;
    if frame.is_empty() {
        return Err(Error::cancelled(15, "protocol input closed"));
    }
    if frame.len() > MAX_REQUEST_BYTES || frame.last() != Some(&b'\n') {
        return Err(Error::usage(
            "protocol frame exceeded 1 MiB or ended before newline",
        ));
    }
    Ok(frame)
}

fn error_envelope(id: &str, error: &Error, signal: Option<i32>) -> Value {
    json!({"type":"error","version":VERSION,"id":id,"error":error_payload(error, signal)})
}

fn error_payload(error: &Error, signal: Option<i32>) -> Value {
    json!({"category":error.kind,"message":error.message,"exit_code":error.exit_code.get(),"signal":signal})
}

fn read_control(reader: &mut impl BufRead, id: &str) -> Result<i32, Error> {
    let frame = read_frame(reader)?;
    let cancel: Cancel = serde_json::from_slice(&frame)
        .map_err(|error| Error::usage(format!("invalid cancellation: {error}")))?;
    if cancel.version != VERSION
        || cancel.message_type != "cancel"
        || cancel.id != id
        || !(1..=64).contains(&cancel.signal)
    {
        return Err(Error::usage("invalid cancellation envelope"));
    }
    Ok(cancel.signal)
}

fn engine_error(error: &EngineError, cancellation: &Cancellation) -> Error {
    if let Some(signal) = cancellation.signal() {
        return Error::cancelled(signal, error.message());
    }
    match error.code() {
        "configuration"
        | "invalid_request"
        | "invalid_attribute_path"
        | "invalid_resource_limit"
        | "invalid_cache_policy"
        | "invalid_substituter"
        | "invalid_public_key"
        | "invalid_out_link" => Error::usage(error.message()),
        _ => Error::external(format!("{}: {}", error.code(), error.message())),
    }
}

fn execute(
    request: &Request,
    runner: &dyn ProcessRunner,
    output: &Arc<Output>,
) -> Result<ResultPayload, Error> {
    let clock = SystemClock;
    let engine = NixEngine::new(
        request.engine_config()?,
        EngineDependencies {
            runner,
            cancellation: &output.cancellation,
            clock: &clock,
            progress: output.as_ref(),
        },
    )
    .map_err(|error| engine_error(&error, &output.cancellation))?;
    let response = engine
        .execute(request.engine_request())
        .map_err(|error| engine_error(&error, &output.cancellation))?;
    match response {
        nix_tools_engine::EngineResponse::Discovery(discovery) => {
            Ok(ResultPayload::Discover { discovery })
        }
        nix_tools_engine::EngineResponse::PreparedRun(prepared) => Ok(ResultPayload::PrepareRun {
            program: prepared.program,
            manifest: prepared.manifest,
        }),
        nix_tools_engine::EngineResponse::Realization(manifest) => match request.operation {
            Operation::Build => Ok(ResultPayload::Build { manifest }),
            Operation::BuildInstallables => Ok(ResultPayload::BuildInstallables { manifest }),
            Operation::Check => Ok(ResultPayload::Check { manifest }),
            Operation::FlakeCheck => {
                let exit_code = match manifest.outcome {
                    nix_tools_engine::ManifestOutcome::Success => 0,
                    nix_tools_engine::ManifestOutcome::Failed => 1,
                    nix_tools_engine::ManifestOutcome::Cancelled => {
                        nix_tools_core::outcome::ExitCode::from_signal(
                            output.cancellation.signal().unwrap_or(2),
                        )
                        .get()
                    }
                };
                Ok(ResultPayload::FlakeCheck {
                    exit_code,
                    manifest,
                })
            }
            Operation::Discover | Operation::PrepareRun => {
                Err(Error::internal("engine returned an unexpected realization"))
            }
        },
    }
}

/// Serves one request on stdin/stdout; the process must exit after this returns.
///
/// # Errors
/// Returns an I/O error if stdout closes before the terminal envelope is delivered.
pub fn serve_stdio() -> Result<(), Error> {
    serve(io::BufReader::new(io::stdin()), false)
}

/// Serves JSON control on inherited fd 3 while the existing TUI owns the terminal.
///
/// # Errors
/// Returns an I/O error if fd 3 or protocol stdout is unavailable.
pub fn serve_interactive() -> Result<(), Error> {
    let control = std::fs::File::open("/dev/fd/3")
        .map_err(|error| Error::io(format!("open engine control fd 3: {error}")))?;
    serve(io::BufReader::new(control), true)
}

fn serve(mut reader: impl BufRead + Send + 'static, interactive: bool) -> Result<(), Error> {
    let cancellation = Cancellation::default();
    crate::forward_termination_signals(&cancellation)?;
    let mut output = Output {
        writer: Mutex::new(io::stdout()),
        cancellation,
        id: String::new(),
        max_response_bytes: DEFAULT_RESPONSE_BYTES,
        failure: Mutex::new(None),
        ui: Mutex::new(None),
    };
    output.send(&json!({"type":"hello","version":VERSION,"capabilities":["discover","build","build_installables","check","prepare_run","flake_check","rebuild","skip_cached","all_outputs","max_jobs","interactive_presentation"],"engine_version":env!("CARGO_PKG_VERSION"),"max_request_bytes":MAX_REQUEST_BYTES,"max_response_bytes":DEFAULT_RESPONSE_BYTES,"cancellation_grace_ms":2000}))?;
    let request = read_frame(&mut reader)
        .and_then(|frame| {
            if let Ok(value) = serde_json::from_slice::<Value>(&frame) {
                value["id"]
                    .as_str()
                    .filter(|id| id.len() <= 128)
                    .unwrap_or_default()
                    .clone_into(&mut output.id);
            }
            serde_json::from_slice::<Request>(&frame)
                .map_err(|error| Error::usage(format!("invalid request: {error}")))
        })
        .and_then(|request| {
            request.validate()?;
            Ok(request)
        });
    let request = match request {
        Ok(request) => request,
        Err(error) => return output.send(&error_envelope(&output.id, &error, None)),
    };
    output.max_response_bytes = request.max_response_bytes;
    if let Some(presentation) = &request.presentation {
        output.ui = Mutex::new(Some(UiSession::detect(
            &presentation.title,
            output.cancellation.clone(),
            if interactive {
                presentation.mode
            } else {
                OutputMode::Stream
            },
        )));
    }
    let output = Arc::new(output);
    let control_error = Arc::new(Mutex::new(None));
    let input_output = Arc::clone(&output);
    let input_error = Arc::clone(&control_error);
    std::thread::Builder::new()
        .name("engine-control".into())
        .spawn(move || match read_control(&mut reader, &input_output.id) {
            Ok(signal) => input_output.cancellation.request(signal),
            Err(error) => {
                if error.kind != ErrorKind::Cancelled {
                    *input_error
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
                }
                input_output.cancellation.request(15);
            }
        })
        .map_err(|error| Error::io(format!("start protocol control reader: {error}")))?;
    let runner = StdProcessRunner::new(Duration::from_millis(20), Redactor::default());
    let result = execute(&request, &runner, &output);
    let control_error = control_error
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    output.terminal(control_error.map_or(result, Err))
}

#[cfg(test)]
#[path = "protocol_test.rs"]
mod protocol_test;
