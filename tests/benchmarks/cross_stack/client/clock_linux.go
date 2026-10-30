//go:build linux

package main

import (
	"fmt"
	"math"
	"syscall"
	"unsafe"
)

func readKernelMonotonic() (int64, error) {
	const clockMonotonic = 1 // Linux CLOCK_MONOTONIC, shared with the sampler.
	var value syscall.Timespec
	_, _, errno := syscall.Syscall(syscall.SYS_CLOCK_GETTIME, clockMonotonic, uintptr(unsafe.Pointer(&value)), 0)
	if errno != 0 {
		return 0, fmt.Errorf("clock_gettime(CLOCK_MONOTONIC): %w", errno)
	}
	seconds, ns := int64(value.Sec), int64(value.Nsec)
	if seconds < 0 || ns < 0 || ns >= 1000000000 || seconds > (math.MaxInt64-ns)/1000000000 {
		return 0, fmt.Errorf("CLOCK_MONOTONIC returned an invalid timespec")
	}
	return seconds*1000000000 + ns, nil
}
