package nixtools

import "syscall"

func waitForExit(pid int) error {
	for {
		// WNOWAIT keeps the child PID reserved until process-group cleanup finishes.
		_, _, err := syscall.Syscall6(syscall.SYS_WAITID, 1, uintptr(pid), 0, syscall.WEXITED|syscall.WNOWAIT, 0, 0)
		if err == syscall.EINTR {
			continue
		}
		if err != 0 {
			return err
		}
		return nil
	}
}
