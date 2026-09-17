package nixtools

import (
	"bufio"
	"context"
	"errors"
	"os"
	"os/exec"
	"syscall"
	"testing"
	"time"
)

func TestReapedLeaderObservationFailureDoesNotSignalGroup(t *testing.T) {
	input, inputWriter, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer input.Close()
	defer inputWriter.Close()
	output, outputWriter, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer output.Close()
	defer outputWriter.Close()
	cmd := exec.Command("/bin/sh", "-c", `exec 3<&0; /bin/sh -c 'echo ready; if IFS= read -r value; then echo "alive:$value"; fi' <&3 & exit 0`)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Stdin, cmd.Stdout = input, outputWriter
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer cmd.Wait()
	input.Close()
	outputWriter.Close()
	lines := make(chan string, 2)
	go func() {
		defer close(lines)
		scanner := bufio.NewScanner(output)
		for scanner.Scan() {
			lines <- scanner.Text()
		}
	}()
	select {
	case line := <-lines:
		if line != "ready" {
			t.Fatalf("unexpected child output: %q", line)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("descendant did not start")
	}
	if _, err := cmd.Process.Wait(); err != nil {
		t.Fatal(err)
	}
	exited := make(chan error, 1)
	exited <- syscall.ECHILD
	err = waitForApp(context.Background(), cmd, exited)
	var failure *Error
	if !errors.As(err, &failure) || failure.Code != "app_wait" || !errors.Is(err, syscall.ECHILD) {
		t.Fatalf("lost reaped-child observation error: %v", err)
	}
	if _, err := inputWriter.Write([]byte("probe\n")); err != nil {
		t.Fatalf("signalled group after leader was reaped: %v", err)
	}
	select {
	case line := <-lines:
		if line != "alive:probe" {
			t.Fatalf("signalled group after leader was reaped: %q", line)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("descendant did not acknowledge probe")
	}
}

func TestExitObservationLeavesChildForWait(t *testing.T) {
	for range 10 {
		cmd := exec.Command("/bin/sh", "-c", "exit 23")
		if err := cmd.Start(); err != nil {
			t.Fatal(err)
		}
		if err := waitForExit(cmd.Process.Pid); err != nil {
			_ = cmd.Process.Kill()
			_ = cmd.Wait()
			t.Fatal(err)
		}
		if err := waitForExit(cmd.Process.Pid); err != nil {
			_ = cmd.Wait()
			t.Fatalf("observed child was already reaped: %v", err)
		}
		if err := cmd.Wait(); ExitCode(err) != 23 {
			t.Fatalf("exit observation consumed status: %v", err)
		}
	}
}

func TestExecuteTerminatesRemainingGroupOnLeaderExit(t *testing.T) {
	ready := make(chan int, 1)
	result := make(chan error, 1)
	go func() {
		result <- (PreparedRun{Program: "/bin/sh"}).Execute(context.Background(), AppOptions{Arguments: []string{"-c", "sleep 60 & echo $!; exit 0"}, Stdout: &pidWriter{ready: ready}})
	}()
	var pid int
	select {
	case pid = <-ready:
	case <-time.After(3 * time.Second):
		t.Fatal("child did not start")
	}
	defer syscall.Kill(pid, syscall.SIGKILL)
	select {
	case err := <-result:
		if err != nil {
			t.Fatalf("leader exit left child holding streams: %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("leader exit did not close its process group")
	}
}

func TestExecuteEscalatesStubbornGroupBeforeReaping(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	ready := make(chan int, 1)
	result := make(chan error, 1)
	go func() {
		result <- (PreparedRun{Program: "/bin/sh"}).Execute(ctx, AppOptions{Arguments: []string{"-c", `trap '' INT TERM; echo $$; while :; do sleep 60; done`}, Stdout: &pidWriter{ready: ready}})
	}()
	var process *os.Process
	select {
	case pid := <-ready:
		var err error
		process, err = os.FindProcess(pid)
		if err != nil {
			t.Fatal(err)
		}
		defer process.Release()
	case <-time.After(3 * time.Second):
		t.Fatal("application did not start")
	}
	start := time.Now()
	cancel()
	select {
	case err := <-result:
		if ExitCode(err) != 137 || time.Since(start) < time.Second {
			t.Fatalf("graceful cancellation did not escalate: %v", err)
		}
		var failure *Error
		if !errors.As(err, &failure) || failure.Code != "cancelled" || !errors.Is(err, context.Canceled) {
			t.Fatalf("forced cancellation lost its category or context cause: %#v", err)
		}
	case <-time.After(4 * time.Second):
		_ = process.Kill()
		t.Fatal("stubborn process group was not killed")
	}
}

func TestObservationFailureCleansOwnedGroupAndKeepsPrecedence(t *testing.T) {
	for _, cancelled := range []bool{false, true} {
		ctx, cancel := context.WithCancel(context.Background())
		ready := make(chan int, 1)
		cmd := exec.Command("/bin/sh", "-c", "sleep 60 & echo $!; wait")
		cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
		cmd.Stdout = &pidWriter{ready: ready}
		cmd.WaitDelay = 2 * time.Second
		if err := cmd.Start(); err != nil {
			cancel()
			t.Fatal(err)
		}
		var childPID int
		select {
		case childPID = <-ready:
		case <-time.After(3 * time.Second):
			_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
			_ = cmd.Wait()
			cancel()
			t.Fatal("descendant did not start")
		}
		if cancelled {
			cancel()
		}
		exited := make(chan error, 1)
		exited <- syscall.EIO
		result := make(chan error, 1)
		go func() { result <- waitForApp(ctx, cmd, exited) }()
		select {
		case err := <-result:
			var failure *Error
			if !errors.As(err, &failure) || failure.Code != "app_wait" || !errors.Is(err, syscall.EIO) || errors.Is(err, exec.ErrWaitDelay) {
				_ = syscall.Kill(childPID, syscall.SIGKILL)
				cancel()
				t.Fatalf("observation failure lost precedence or left a descendant holding output: %#v", err)
			}
		case <-time.After(3 * time.Second):
			_ = syscall.Kill(childPID, syscall.SIGKILL)
			cancel()
			t.Fatal("observation failure did not stop the group")
		}
		cancel()
		needsCleanup := true
		defer func() {
			if needsCleanup {
				_ = syscall.Kill(childPID, syscall.SIGKILL)
			}
		}()
		assertProcessStopped(t, childPID)
		needsCleanup = false
	}
}
