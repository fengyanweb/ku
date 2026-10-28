//go:build linux

package main

import (
	"errors"
	"syscall"
	"testing"
)

func TestDescriptorTableTargetUsesConfiguredLiveConnectionsAndSoftLimit(t *testing.T) {
	for _, c := range []struct {
		name        string
		maxFD       uint64
		concurrency int
		limit       uint64
		target      uint64
		clamped     bool
		wantError   bool
	}{
		{name: "configured4096", maxFD: 11, concurrency: 4096, limit: 10000, target: 4171},
		{name: "softClamp", maxFD: 10, concurrency: 32, limit: 64, target: 63, clamped: true},
		{name: "maxFDOverflow", maxFD: ^uint64(0) - 2, concurrency: 1, limit: 1024, target: 1023, clamped: true},
		{name: "loweredSoftLimit", maxFD: 100, concurrency: 1, limit: 32, target: 31, clamped: true},
		{name: "infiniteLimitStillUsesCIntFD", maxFD: 1<<31 - 2, concurrency: 8, limit: ^uint64(0), target: 1<<31 - 1, clamped: true},
		{name: "zeroLimit", concurrency: 1, wantError: true},
		{name: "zeroConcurrency", limit: 1024, wantError: true},
	} {
		t.Run(c.name, func(t *testing.T) {
			target, clamped, err := descriptorTableTarget(c.maxFD, c.concurrency, c.limit)
			if (err != nil) != c.wantError || target != c.target || clamped != c.clamped {
				t.Fatalf("target=%d clamped=%v err=%v; want %d/%v/error=%v", target, clamped, err, c.target, c.clamped, c.wantError)
			}
		})
	}
}

func TestDescriptorPreparationDuplicatesOnceAndClosesOnlyReturnedDescriptor(t *testing.T) {
	p := descriptorPreparation{maxOpenFD: 9, configuredConnections: 8, softLimit: 256, duplicateFD: -1}
	duplicates, closes := 0, 0
	p, err := duplicateDescriptorForPreparation(p, 3, func(source, target int) (int, error) {
		duplicates++
		if source != 3 || target != 81 {
			t.Fatalf("unexpected duplicate source/target %d/%d", source, target)
		}
		return 85, nil // Existing FDs at/above the requested minimum are skipped.
	}, func(fd int) error {
		closes++
		if fd != 85 {
			t.Fatalf("attempted to close an unowned descriptor %d", fd)
		}
		return nil
	})
	if err != nil || duplicates != 1 || closes != 1 || p.targetFD != 81 || p.duplicateFD != 85 || !p.duplicateClosed || p.limitClamped {
		t.Fatalf("unexpected preparation %+v duplicate=%d close=%d err=%v", p, duplicates, closes, err)
	}
}

func TestDescriptorPreparationFailuresAreReturnedWithoutRetryOrUnownedClose(t *testing.T) {
	for _, c := range []struct {
		name     string
		dupErr   error
		closeErr error
		closes   int
	}{
		{name: "duplicateFailure", dupErr: syscall.EMFILE},
		{name: "closeFailure", closeErr: syscall.EINTR, closes: 1},
	} {
		t.Run(c.name, func(t *testing.T) {
			duplicates, closes := 0, 0
			p, err := duplicateDescriptorForPreparation(descriptorPreparation{maxOpenFD: 9, configuredConnections: 8, softLimit: 256, duplicateFD: -1}, 3, func(source, target int) (int, error) {
				duplicates++
				return 85, c.dupErr
			}, func(fd int) error {
				closes++
				if fd != 85 {
					t.Fatalf("unowned close %d", fd)
				}
				return c.closeErr
			})
			want := c.dupErr
			if want == nil {
				want = c.closeErr
			}
			if !errors.Is(err, want) || duplicates != 1 || closes != c.closes || p.duplicateClosed {
				t.Fatalf("preparation=%+v duplicate=%d close=%d err=%v", p, duplicates, closes, err)
			}
		})
	}
}
