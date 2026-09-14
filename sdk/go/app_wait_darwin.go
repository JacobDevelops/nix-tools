package nixtools

import "syscall"

func waitForExit(pid int) error {
	syscall.ForkLock.RLock()
	queue, err := syscall.Kqueue()
	if err == nil {
		syscall.CloseOnExec(queue)
	}
	syscall.ForkLock.RUnlock()
	if err != nil {
		return err
	}
	defer syscall.Close(queue)
	change := syscall.Kevent_t{Ident: uint64(pid), Filter: syscall.EVFILT_PROC, Flags: syscall.EV_ADD | syscall.EV_ONESHOT, Fflags: syscall.NOTE_EXIT}
	for {
		_, err = syscall.Kevent(queue, []syscall.Kevent_t{change}, nil, nil)
		if err != syscall.EINTR {
			break
		}
	}
	// This owned child has not been reaped, so ESRCH means it already exited.
	if err == syscall.ESRCH {
		return nil
	}
	if err != nil {
		return err
	}
	events := make([]syscall.Kevent_t, 1)
	for {
		count, err := syscall.Kevent(queue, nil, events, nil)
		if err == syscall.EINTR {
			continue
		}
		if err != nil {
			return err
		}
		if count > 0 {
			if events[0].Flags&syscall.EV_ERROR != 0 {
				return syscall.Errno(events[0].Data)
			}
			return nil
		}
	}
}
