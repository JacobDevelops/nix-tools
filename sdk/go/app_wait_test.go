package nixtools

import (
	"context"
	"os"
	"os/exec"
	"syscall"
	"testing"
	"time"
)

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
	case <-time.After(4 * time.Second):
		_ = process.Kill()
		t.Fatal("stubborn process group was not killed")
	}
}
