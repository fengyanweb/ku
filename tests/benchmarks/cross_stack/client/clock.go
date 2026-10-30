package main

import (
	"math"
	"time"
)

type monotonicAnchor struct {
	originNS, uncertaintyNS int64
	reason                  string
}

// The later Go timestamp remains the arrival origin. The kernel read lies
// inside the bracket; shifting it by half the Go-monotonic bracket estimates
// that later origin, with the full bracket retained as a conservative bound.
func captureMonotonicAnchor(now func() time.Time, read func() (int64, error)) (time.Time, monotonicAnchor) {
	before := now()
	kernelNS, err := read()
	origin := now()
	if err != nil {
		return origin, monotonicAnchor{reason: err.Error()}
	}
	bracketNS := origin.Sub(before).Nanoseconds()
	if bracketNS < 0 || kernelNS < 0 || kernelNS > math.MaxInt64-bracketNS/2 {
		return origin, monotonicAnchor{reason: "invalid CLOCK_MONOTONIC anchor bracket or range"}
	}
	return origin, monotonicAnchor{originNS: kernelNS + bracketNS/2, uncertaintyNS: bracketNS}
}

func (a monotonicAnchor) at(offset any) any {
	ns, ok := offset.(int64)
	if a.reason != "" || !ok || (ns > 0 && a.originNS > math.MaxInt64-ns) || (ns < 0 && a.originNS < math.MinInt64-ns) {
		return nil
	}
	return a.originNS + ns
}

func (a monotonicAnchor) annotate(clocks map[string]any) {
	clocks["origin_monotonic_ns"] = a.at(int64(0))
	clocks["origin_monotonic_uncertainty_ns"] = nil
	clocks["origin_monotonic_status"] = "unavailable"
	clocks["origin_monotonic_reason"] = a.reason
	if a.reason == "" {
		clocks["origin_monotonic_uncertainty_ns"] = a.uncertaintyNS
		clocks["origin_monotonic_status"] = "available"
		clocks["origin_monotonic_reason"] = nil
	}
	for _, name := range []string{"planned_cutoff", "last_actual_sent", "last_actual_completed", "last_complete_response"} {
		clocks[name+"_monotonic_ns"] = a.at(clocks[name+"_offset_ns"])
	}
}
