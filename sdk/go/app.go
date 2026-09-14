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

// Execute supervises an isolated process group; use Exec for an interactive terminal.
func (p PreparedRun) Execute(ctx context.Context, options AppOptions) error {
	if ctx.Err() != nil {
		return cancellationError(ctx)
	}
	cmd := exec.CommandContext(ctx, p.Program, options.Arguments...)
	cmd.Env = options.Environment
	cmd.Dir = options.WorkingDirectory
	cmd.Stdin = options.Stdin
	cmd.Stdout = options.Stdout
	cmd.Stderr = options.Stderr
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	done := make(chan struct{})
	defer close(done)
	cmd.Cancel = func() error {
		pid := -cmd.Process.Pid
		go func() {
			select {
			case <-done:
			case <-time.After(2 * time.Second):
				_ = syscall.Kill(pid, syscall.SIGKILL)
			}
		}()
		return syscall.Kill(pid, cancellationSignal(ctx))
	}
	cmd.WaitDelay = 2 * time.Second
	err := cmd.Run()
	if ctx.Err() != nil && cmd.Process != nil {
		_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
	}
	if err != nil {
		if ctx.Err() != nil && errors.Is(err, ctx.Err()) {
			return cancellationError(ctx)
		}
		status := 1
		if exit, ok := err.(*exec.ExitError); ok {
			status = exit.ExitCode()
			if ws, ok := exit.Sys().(syscall.WaitStatus); ok && ws.Signaled() {
				status = 128 + int(ws.Signal())
			}
		}
		return &Error{Code: "app_exit", Message: err.Error(), Status: status, Cause: err}
	}
	return nil
}
