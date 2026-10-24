//go:build !linux

package main

import (
	"fmt"
	"runtime"
)

func readKernelMonotonic() (int64, error) {
	return 0, fmt.Errorf("Linux CLOCK_MONOTONIC anchor unavailable on %s", runtime.GOOS)
}
