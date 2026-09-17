package nixtools

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"syscall"
	"time"
)

type SignalCause struct{ Signal syscall.Signal }

func (s SignalCause) Error() string { return fmt.Sprintf("signal %d", s.Signal) }
func cancellationSignal(ctx context.Context) syscall.Signal {
	if cause, ok := context.Cause(ctx).(SignalCause); ok && cause.Signal > 0 {
		return cause.Signal
	}
	return syscall.SIGINT
}

func cancellationError(ctx context.Context) *Error {
	status := 128 + int(cancellationSignal(ctx))
	if errors.Is(ctx.Err(), context.DeadlineExceeded) {
		status = 124
	}
	return &Error{Code: "cancelled", Message: ctx.Err().Error(), Status: status, Cause: ctx.Err()}
}

func ExitCode(err error) int {
	if err == nil {
		return 0
	}
	var status interface{ ExitCode() int }
	if errors.As(err, &status) {
		return status.ExitCode()
	}
	if errors.Is(err, context.Canceled) {
		return 130
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return 124
	}
	return 1
}

type AppOptions struct {
	Arguments        []string
	Environment      []string
	WorkingDirectory string
	Stdin            io.Reader
	Stdout           io.Writer
	Stderr           io.Writer
}

// Exec replaces the caller, preserving its terminal and signals; success never returns.
func (p PreparedRun) Exec(arguments []string, environment []string, workingDirectory string) error {
	if environment == nil {
		environment = os.Environ()
	}
	if workingDirectory != "" {
		original, err := os.Open(".")
		if err != nil {
			return err
		}
		defer original.Close()
		if err = os.Chdir(workingDirectory); err != nil {
			return err
		}
		err = syscall.Exec(p.Program, append([]string{p.Program}, arguments...), environment)
		return errors.Join(err, original.Chdir())
	}
	return syscall.Exec(p.Program, append([]string{p.Program}, arguments...), environment)
}

// Execute owns the process group's lifetime; use Exec for native handoff or an interactive terminal.
func (p PreparedRun) Execute(ctx context.Context, options AppOptions) error {
	if ctx.Err() != nil {
		return cancellationError(ctx)
	}
	cmd := exec.Command(p.Program, options.Arguments...)
	cmd.Env = options.Environment
	cmd.Dir = options.WorkingDirectory
	cmd.Stdin = options.Stdin
	cmd.Stdout = options.Stdout
	cmd.Stderr = options.Stderr
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.WaitDelay = 2 * time.Second
	if err := cmd.Start(); err != nil {
		return &Error{Code: "app_exit", Message: err.Error(), Cause: err}
	}
	exited := make(chan error, 1)
	go func() { exited <- waitForExit(cmd.Process.Pid) }()
	return waitForApp(ctx, cmd, exited)
}

func waitForApp(ctx context.Context, cmd *exec.Cmd, exited <-chan error) error {
	var observationError error
	cancelled := false
	groupKilled := false
	select {
	case observationError = <-exited:
	case <-ctx.Done():
		cancelled = true
		_ = syscall.Kill(-cmd.Process.Pid, cancellationSignal(ctx))
		timer := time.NewTimer(2 * time.Second)
		// Reap only after the last group signal, so its ID cannot target a reused PID.
		select {
		case observationError = <-exited:
			timer.Stop()
		case <-timer.C:
			_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
			groupKilled = true
			observationError = <-exited
		}
	}
	if observationError != nil {
		if !errors.Is(observationError, syscall.ECHILD) {
			_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		}
		_ = cmd.Process.Kill()
	} else if !groupKilled {
		_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
	}
	err := cmd.Wait()
	if observationError != nil {
		return &Error{Code: "app_wait", Message: observationError.Error(), Cause: errors.Join(observationError, err)}
	}
	if cancelled && err == nil {
		return cancellationError(ctx)
	}
	if err != nil {
		if ctx.Err() != nil && errors.Is(err, ctx.Err()) {
			return cancellationError(ctx)
		}
		status := 1
		exit, hasExitStatus := err.(*exec.ExitError)
		if hasExitStatus {
			status = exit.ExitCode()
			if ws, ok := exit.Sys().(syscall.WaitStatus); ok && ws.Signaled() {
				status = 128 + int(ws.Signal())
			}
		}
		if cancelled {
			failure := cancellationError(ctx)
			if hasExitStatus {
				failure.Status = status
			}
			return failure
		}
		return &Error{Code: "app_exit", Message: err.Error(), Status: status, Cause: err}
	}
	return nil
}
