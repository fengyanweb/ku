//go:build linux

package main

import (
	"os"
	"runtime"
	"strconv"
	"strings"
	"syscall"
)

func admissionProcessSnapshot() map[string]any {
	var memory runtime.MemStats
	runtime.ReadMemStats(&memory)
	result := map[string]any{"pid": os.Getpid(), "goroutines": runtime.NumGoroutine(), "go_heap_alloc_bytes": memory.HeapAlloc, "go_sys_bytes": memory.Sys}
	if descriptors, err := os.ReadDir("/proc/self/fd"); err != nil {
		result["fd_error"] = err.Error()
	} else {
		result["open_fd_count_including_snapshot_directory"] = len(descriptors)
	}
	var usage syscall.Rusage
	if err := syscall.Getrusage(syscall.RUSAGE_SELF, &usage); err != nil {
		result["usage_error"] = err.Error()
	} else {
		result["user_cpu_us"] = usage.Utime.Sec*1000000 + int64(usage.Utime.Usec)
		result["system_cpu_us"] = usage.Stime.Sec*1000000 + int64(usage.Stime.Usec)
		result["max_rss_kib"] = usage.Maxrss
		result["minor_faults"], result["major_faults"] = usage.Minflt, usage.Majflt
		result["voluntary_context_switches"], result["involuntary_context_switches"] = usage.Nvcsw, usage.Nivcsw
	}
	if raw, err := os.ReadFile("/proc/self/status"); err != nil {
		result["status_error"] = err.Error()
	} else {
		for _, line := range strings.Split(string(raw), "\n") {
			fields := strings.Fields(line)
			if len(fields) < 2 {
				continue
			}
			switch fields[0] {
			case "Threads:", "VmRSS:", "VmSize:", "VmSwap:":
				if value, e := strconv.ParseUint(fields[1], 10, 64); e == nil {
					key := strings.TrimSuffix(fields[0], ":")
					if fields[0] != "Threads:" {
						key += "_kib"
					}
					result[key] = value
				}
			}
		}
	}
	return result
}
