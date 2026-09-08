use nix_tools_core::outcome::{Error, Result};
use nix_tools_core::process::{Cancellation, ProcessSpec};

pub(crate) fn replace_process(spec: &ProcessSpec, cancellation: &Cancellation) -> Result<()> {
    if let Some(signal) = cancellation.signal() {
        return Err(Error::cancelled(signal, "app execution cancelled"));
    }
    replace(spec)
}

#[cfg(unix)]
fn replace(spec: &ProcessSpec) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(&spec.env)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    let error = command.exec();
    Err(Error::io(format!(
        "execute {}: {error}",
        spec.program.to_string_lossy()
    )))
}

#[cfg(not(unix))]
fn replace(_spec: &ProcessSpec) -> Result<()> {
    Err(Error::usage(
        "exec requires Unix; select supervised app execution",
    ))
}

#[cfg(all(test, unix))]
#[path = "app_exec_test.rs"]
mod tests;
