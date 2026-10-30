package main

import (
	"errors"
	"math"
	"runtime"
	"strings"
	"testing"
	"time"
)

func TestMonotonicAnchorBracketAndConservativeError(t *testing.T) {
	before := time.Now()
	width := 200001 * time.Nanosecond
	after := before.Add(width)
	var calls []string
	origin, anchor := captureMonotonicAnchor(func() time.Time {
		calls = append(calls, "now")
		if len(calls) == 1 {
			return before
		}
		return after
	}, func() (int64, error) {
		calls = append(calls, "kernel")
		return 1000000000, nil
	})
	if strings.Join(calls, "/") != "now/kernel/now" || origin != after || anchor.reason != "" || anchor.originNS != 1000000000+width.Nanoseconds()/2 || anchor.uncertaintyNS != width.Nanoseconds() {
		t.Fatalf("wrong origin/read bracket: %v %+v %v", origin, anchor, calls)
	}
	for _, readPosition := range []int64{0, 1, width.Nanoseconds() / 2, width.Nanoseconds() - 1, width.Nanoseconds()} {
		trueOrigin := int64(1000000000) + width.Nanoseconds() - readPosition
		err := anchor.originNS - trueOrigin
		if err < -anchor.uncertaintyNS || err > anchor.uncertaintyNS {
			t.Fatalf("read position %d exceeds declared error bound: %d", readPosition, err)
		}
	}
}

func TestUnavailableMonotonicAnchorIsExplicitNull(t *testing.T) {
	before := time.Now()
	after := before.Add(time.Microsecond)
	count := 0
	origin, anchor := captureMonotonicAnchor(func() time.Time {
		count++
		if count == 1 {
			return before
		}
		return after
	}, func() (int64, error) { return 0, errors.New("clock unavailable for test") })
	clocks := map[string]any{"planned_cutoff_offset_ns": int64(1000), "last_actual_sent_offset_ns": int64(2), "last_actual_completed_offset_ns": int64(3), "last_complete_response_offset_ns": int64(4)}
	anchor.annotate(clocks)
	if origin != after || clocks["origin_monotonic_status"] != "unavailable" || clocks["origin_monotonic_reason"] != "clock unavailable for test" {
		t.Fatal(clocks)
	}
	for _, key := range []string{"origin_monotonic_ns", "origin_monotonic_uncertainty_ns", "planned_cutoff_monotonic_ns", "last_actual_sent_monotonic_ns", "last_actual_completed_monotonic_ns", "last_complete_response_monotonic_ns"} {
		if clocks[key] != nil {
			t.Fatalf("missing clock became a numeric timestamp: %s=%v", key, clocks[key])
		}
	}
}

func TestMonotonicDerivedClocksIgnoreWallJumpsAndMissingEvents(t *testing.T) {
	anchor := monotonicAnchor{originNS: 1000000000000, uncertaintyNS: 1000}
	clocks := map[string]any{
		"origin_unix_ns": int64(1792693515989644523), "planned_cutoff_unix_ns": int64(1792693520989644523),
		"last_actual_sent_unix_ns": int64(1792693521369313898), "last_actual_completed_unix_ns": int64(1792693521489305368), "last_complete_response_unix_ns": nil,
		"planned_cutoff_offset_ns": int64(5000000000), "last_actual_sent_offset_ns": int64(5379669418), "last_actual_completed_offset_ns": int64(5499660846), "last_complete_response_offset_ns": nil,
	}
	anchor.annotate(clocks)
	want := map[string]any{
		"origin_monotonic_ns": anchor.originNS, "planned_cutoff_monotonic_ns": anchor.originNS + 5000000000,
		"last_actual_sent_monotonic_ns": anchor.originNS + 5379669418, "last_actual_completed_monotonic_ns": anchor.originNS + 5499660846,
		"last_complete_response_monotonic_ns": nil,
	}
	// Reproduce a backwards wall-clock relation while preserving the measured
	// Go-monotonic offsets. Annotation must never reconstruct from Unix deltas.
	clocks["origin_unix_ns"] = clocks["origin_unix_ns"].(int64) + int64(time.Hour)
	clocks["last_actual_sent_unix_ns"] = clocks["last_actual_sent_unix_ns"].(int64) - 269305973
	clocks["last_actual_completed_unix_ns"] = clocks["last_actual_completed_unix_ns"].(int64) - int64(2*time.Hour)
	anchor.annotate(clocks)
	for key, value := range want {
		if clocks[key] != value {
			t.Fatalf("wall clock or missing event altered %s: got %v want %v", key, clocks[key], value)
		}
	}
	if anchor.at(int64(math.MaxInt64)) != nil || anchor.at(nil) != nil {
		t.Fatal("invalid offset became a wrapped or invented timestamp")
	}
}

func TestPlatformMonotonicAnchorAvailability(t *testing.T) {
	origin, first := captureMonotonicAnchor(time.Now, readKernelMonotonic)
	clocks := map[string]any{"planned_cutoff_offset_ns": int64(time.Second)}
	first.annotate(clocks)
	if runtime.GOOS != "linux" {
		if clocks["origin_monotonic_status"] != "unavailable" || clocks["origin_monotonic_ns"] != nil || clocks["origin_monotonic_uncertainty_ns"] != nil || clocks["planned_cutoff_monotonic_ns"] != nil || !strings.Contains(first.reason, runtime.GOOS) {
			t.Fatalf("non-Linux run claimed Linux timing: %#v", clocks)
		}
		return
	}
	otherOrigin, second := captureMonotonicAnchor(time.Now, readKernelMonotonic)
	if first.reason != "" || second.reason != "" || first.originNS <= 0 || clocks["origin_monotonic_status"] != "available" {
		t.Fatalf("Linux kernel clock unavailable: first=%+v second=%+v", first, second)
	}
	err := (second.originNS - first.originNS) - otherOrigin.Sub(origin).Nanoseconds()
	bound := first.uncertaintyNS + second.uncertaintyNS
	if err < -bound || err > bound {
		t.Fatalf("kernel anchor differs from Go elapsed time beyond brackets: error=%dns bound=%dns", err, bound)
	}
}
