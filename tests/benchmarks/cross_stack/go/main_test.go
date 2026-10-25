package main

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// The gate observes a real checksum boundary. It does not replace the kernel
// or publish a synthetic worker result; tests release the actual worker.
type checkedCPUContext struct {
	context.Context
	checks  atomic.Int64
	gateAt  int64
	entered chan struct{}
	proceed chan struct{}
}

func (c *checkedCPUContext) Err() error {
	if c.checks.Add(1) == c.gateAt {
		close(c.entered)
		<-c.proceed
	}
	return c.Context.Err()
}

func TestChecksumContextMatchesUnchangedBatchKernel(t *testing.T) {
	for _, seed := range []int64{3, 11} {
		for _, iterations := range []int64{1000, 1023, 1024, 1025, 32000, 512000, 1000000} {
			got, err := checksumContext(context.Background(), iterations, seed)
			if want := checksum(iterations, seed); err != nil || got != want {
				t.Fatalf("iterations=%d seed=%d: got=%d want=%d err=%v", iterations, seed, got, want, err)
			}
		}
	}
}

func TestChecksumContextPreCanceledStopsAtInitialCheck(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	checked := &checkedCPUContext{Context: ctx}
	value, err := checksumContext(checked, 512000, 3)
	if !errors.Is(err, context.Canceled) || value != 0 || checked.checks.Load() != 1 {
		t.Fatalf("pre-canceled result=%d checks=%d err=%v", value, checked.checks.Load(), err)
	}
}

func TestCPUWorkerRunningCancelRetainsReservationUntilActualExit(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	checked := &checkedCPUContext{Context: ctx, gateAt: 2, entered: make(chan struct{}), proceed: make(chan struct{})}
	job := cpuJob{512000, 11, make(chan cpuResult, 1), checked}
	s := &service{cpu: make(chan cpuJob, 1)}
	s.actualCPUJobs.Add(1)
	s.cpu <- job
	close(s.cpu)
	done := make(chan struct{})
	go func() { s.cpuWorker(); close(done) }()
	var release sync.Once
	t.Cleanup(func() {
		cancel()
		release.Do(func() { close(checked.proceed) })
		select {
		case <-done:
		case <-time.After(2 * time.Second):
			t.Error("owned CPU worker did not exit")
		}
	})
	select {
	case <-checked.entered: // The first real 512-iteration chunk has finished.
	case <-time.After(2 * time.Second):
		t.Fatal("worker never reached its next cancellation boundary")
	}
	cancel()
	if got := s.actualCPUJobs.Load(); got != 1 {
		t.Fatalf("running job reservation returned before worker exit: %d", got)
	}
	select {
	case <-job.reply:
		t.Fatal("worker published a result while its real operation was still gated")
	default:
	}
	release.Do(func() { close(checked.proceed) })
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("canceled worker did not actually exit")
	}
	result := <-job.reply
	if !errors.Is(result.err, context.Canceled) || result.value != 0 || checked.checks.Load() != 2 || s.actualCPUJobs.Load() != 0 {
		t.Fatalf("actual canceled exit: result=%+v checks=%d resident=%d", result, checked.checks.Load(), s.actualCPUJobs.Load())
	}
}

func TestCPUWorkDeadlineIncludesQueueAndCancelDoesNotRefundQueuedJob(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	s := &service{cpu: make(chan cpuJob, 1)}
	request := httptest.NewRequest(http.MethodGet, "/work?rid=2&work="+strings.Repeat("w", 512)+"&seed=sss", nil).WithContext(ctx)
	response := httptest.NewRecorder()
	done := make(chan struct{})
	started := time.Now()
	go func() { s.serve(response, request); close(done) }()
	t.Cleanup(func() {
		cancel()
		select {
		case <-done:
			close(s.cpu)
			s.cpuWorker() // Physically remove any still-owned canceled queue entry.
			if got := s.actualCPUJobs.Load(); got != 0 {
				t.Errorf("removed canceled job still owns reservation: %d", got)
			}
		case <-time.After(2 * time.Second):
			t.Error("owned HTTP handler did not exit")
		}
	})
	var job cpuJob
	select {
	case job = <-s.cpu:
	case <-time.After(2 * time.Second):
		t.Fatal("CPU request was not admitted")
	}
	queued := time.Now()
	s.cpu <- job
	deadline, ok := job.context.Deadline()
	if !ok || deadline.Before(started.Add(cpuRequestTimeout)) || deadline.After(queued.Add(cpuRequestTimeout)) {
		t.Fatalf("request-entry deadline missing or restarted: start=%v queued=%v deadline=%v", started, queued, deadline)
	}
	cancel()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("canceled HTTP waiter did not return")
	}
	if response.Code != 504 || response.Body.String() != "deadline" || s.actualCPUJobs.Load() != 1 || len(s.cpu) != 1 {
		t.Fatalf("queued cancellation returned physical reservation: status=%d body=%q resident=%d queued=%d", response.Code, response.Body.String(), s.actualCPUJobs.Load(), len(s.cpu))
	}
}

func TestCPUQueueFullRefundsOnlyRejectedReservation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	s := &service{cpu: make(chan cpuJob, 1)}
	s.actualCPUJobs.Add(1)
	s.cpu <- cpuJob{1000, 3, make(chan cpuResult, 1), ctx}
	t.Cleanup(func() {
		close(s.cpu)
		s.cpuWorker()
		if got := s.actualCPUJobs.Load(); got != 0 {
			t.Errorf("removed canceled queue entry still owns reservation: %d", got)
		}
	})
	response := httptest.NewRecorder()
	s.serve(response, httptest.NewRequest(http.MethodGet, "/work?rid=2&work=w&seed=sss", nil))
	if response.Code != 503 || response.Body.String() != "cpu_queue_full" || s.actualCPUJobs.Load() != 1 || len(s.cpu) != 1 {
		t.Fatalf("full queue reservation mismatch: status=%d body=%q resident=%d queued=%d", response.Code, response.Body.String(), s.actualCPUJobs.Load(), len(s.cpu))
	}
}

func TestCPUHTTPResponseRetainsCommonKernelAndFormatting(t *testing.T) {
	s := &service{cpu: make(chan cpuJob, 1), cpuWorkers: 1}
	done := make(chan struct{})
	go func() { s.cpuWorker(); close(done) }()
	t.Cleanup(func() {
		close(s.cpu)
		select {
		case <-done:
		case <-time.After(2 * time.Second):
			t.Error("owned CPU worker did not stop")
		}
		if got := s.actualCPUJobs.Load(); got != 0 {
			t.Errorf("finished HTTP computation leaked reservation: %d", got)
		}
	})
	for _, seed := range []int64{3, 11} {
		response := httptest.NewRecorder()
		s.serve(response, httptest.NewRequest(http.MethodGet, "/work?rid=2&work="+strings.Repeat("w", 512)+"&seed="+strings.Repeat("s", int(seed)), nil))
		want := fmt.Sprintf("2:%08d", checksum(512000, seed))
		if response.Code != 200 || response.Body.String() != want || response.Header().Get("Content-Length") != fmt.Sprint(len(want)) {
			t.Fatalf("common CPU response seed=%d: status=%d body=%q want=%q", seed, response.Code, response.Body.String(), want)
		}
	}
}
