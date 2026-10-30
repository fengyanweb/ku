//go:build !linux

package main

import "time"

func prepareDescriptorTable(concurrency int) (descriptorPreparation, error) {
	started := time.Now()
	return descriptorPreparation{supported: false, method: "unsupported_non_linux_noop", configuredConnections: concurrency, duplicateFD: -1, started: started, finished: time.Now()}, nil
}
