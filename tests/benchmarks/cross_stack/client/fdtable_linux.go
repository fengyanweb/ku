//go:build linux

package main

import (
	"fmt"
	"os"
	"strconv"
	"syscall"
	"time"
)

// Covers the Go poller, report/event files and small runtime bookkeeping beyond
// the actual configured maximum of one live connection per request worker.
const descriptorRuntimeReserveFDs uint64 = 64

func descriptorTableTarget(maxOpenFD uint64, concurrency int, softLimit uint64) (uint64, bool, error) {
	if concurrency < 1 || softLimit == 0 {
		return 0, false, fmt.Errorf("invalid concurrency %d or RLIMIT_NOFILE soft limit %d", concurrency, softLimit)
	}
	// fcntl's descriptor argument is a signed C int, including on Linux/64.
	ceiling := min(softLimit-1, uint64(1<<31-1))
	extra := uint64(concurrency) + descriptorRuntimeReserveFDs
	// Subtraction first also handles a lowered soft limit below an existing FD
	// and avoids wrapping maxOpenFD+extra for a malformed/injected boundary.
	if maxOpenFD > ceiling || extra > ceiling-maxOpenFD {
		return ceiling, true, nil
	}
	return maxOpenFD + extra, false, nil
}

func duplicateDescriptorForPreparation(p descriptorPreparation, sourceFD int, duplicate func(int, int) (int, error), closeFD func(int) error) (descriptorPreparation, error) {
	target, clamped, err := descriptorTableTarget(p.maxOpenFD, p.configuredConnections, p.softLimit)
	if err != nil {
		return p, err
	}
	p.targetFD, p.limitClamped = target, clamped
	p.duplicateFD, err = duplicate(sourceFD, int(target))
	if err != nil {
		return p, fmt.Errorf("F_DUPFD_CLOEXEC target %d soft limit %d: %w", target, p.softLimit, err)
	}
	// Never dup3 onto a chosen number or retry close: F_DUPFD_CLOEXEC selects a
	// free descriptor; a close error must not risk closing a reused descriptor.
	err = closeFD(p.duplicateFD)
	p.duplicateClosed = err == nil
	if err != nil {
		return p, fmt.Errorf("close prepared duplicate %d: %w", p.duplicateFD, err)
	}
	return p, nil
}

func prepareDescriptorTable(concurrency int) (p descriptorPreparation, err error) {
	p = descriptorPreparation{supported: true, method: "linux_F_DUPFD_CLOEXEC_once", configuredConnections: concurrency, runtimeReserveFDs: descriptorRuntimeReserveFDs, duplicateFD: -1, started: time.Now()}
	defer func() { p.finished = time.Now() }()
	var limit syscall.Rlimit
	if err = syscall.Getrlimit(syscall.RLIMIT_NOFILE, &limit); err != nil {
		return p, fmt.Errorf("read RLIMIT_NOFILE: %w", err)
	}
	p.softLimit = limit.Cur
	// Own the source so closed stdin or concurrent use of an application FD
	// cannot affect preparation. This opens no socket and issues no request.
	source, err := os.Open(os.DevNull)
	if err != nil {
		return p, fmt.Errorf("open descriptor preparation source: %w", err)
	}
	defer func() {
		closeErr := source.Close()
		p.sourceClosed = closeErr == nil
		if closeErr != nil {
			if err != nil {
				err = fmt.Errorf("%w; close preparation source: %v", err, closeErr)
			} else {
				err = fmt.Errorf("close preparation source: %w", closeErr)
			}
		}
	}()
	entries, err := os.ReadDir("/proc/self/fd")
	if err != nil {
		return p, fmt.Errorf("read current descriptor maximum: %w", err)
	}
	for _, entry := range entries {
		fd, parseErr := strconv.ParseUint(entry.Name(), 10, 64)
		if parseErr != nil {
			return p, fmt.Errorf("parse current descriptor %q: %w", entry.Name(), parseErr)
		}
		p.maxOpenFD = max(p.maxOpenFD, fd)
	}
	return duplicateDescriptorForPreparation(p, int(source.Fd()), func(fd, target int) (int, error) {
		// Unlike RawSyscall in the socket path, Syscall enters the scheduler's
		// syscall state so kernel FD-table allocation/RCU waiting releases the P.
		value, _, errno := syscall.Syscall(syscall.SYS_FCNTL, uintptr(fd), uintptr(syscall.F_DUPFD_CLOEXEC), uintptr(target))
		if errno != 0 {
			return -1, errno
		}
		return int(value), nil
	}, syscall.Close)
}
