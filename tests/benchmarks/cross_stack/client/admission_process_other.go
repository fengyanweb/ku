//go:build !linux

package main

import (
	"os"
	"runtime"
)

func admissionProcessSnapshot() map[string]any {
	var memory runtime.MemStats
	runtime.ReadMemStats(&memory)
	return map[string]any{"pid": os.Getpid(), "goroutines": runtime.NumGoroutine(), "go_heap_alloc_bytes": memory.HeapAlloc, "go_sys_bytes": memory.Sys, "cpu_rss_scope": "Linux /proc and getrusage only; external observation required on this platform"}
}
