// capacity_client is a bounded, dependency-free HTTP/1.1 acceptance client.
// Each worker owns at most one connection and one request. It deliberately uses
// Request.Write/ReadResponse rather than Transport: Transport may transparently
// replay GET requests after a reused connection fails. This client never does.
package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math"
	"net"
	"net/http"
	"net/url"
	"os"
	"runtime"
	"strings"
	"sync"
	"time"
)

const histogramBuckets = 100001 // 0.1 ms, final bucket includes >= 10 seconds.
const maxResponseHeaderBytes = 32768
const errorSampleLimit = 24
const errorSamplesPerCategory = 4

type histogram struct {
	buckets [histogramBuckets]uint64
	count   uint64
	sumNS   int64
	maxNS   int64
}

func (h *histogram) add(d time.Duration) {
	if d < 0 {
		d = 0
	}
	n := int64(d)
	i := n / int64(100*time.Microsecond)
	if i >= histogramBuckets {
		i = histogramBuckets - 1
	}
	h.buckets[i]++
	h.count++
	h.sumNS += n
	if n > h.maxNS {
		h.maxNS = n
	}
}

func (h *histogram) value() map[string]any {
	percentile := func(q float64) float64 {
		if h.count == 0 {
			return 0
		}
		target := uint64(math.Ceil(float64(h.count) * q))
		var total uint64
		for i, n := range h.buckets {
			total += n
			if total >= target {
				return float64(i+1) / 10
			}
		}
		return 0
	}
	average := 0.0
	if h.count > 0 {
		average = float64(h.sumNS) / float64(h.count) / 1e6
	}
	return map[string]any{"count": h.count, "p50_ms": percentile(.50), "p95_ms": percentile(.95),
		"p99_ms": percentile(.99), "max_ms": float64(h.maxNS) / 1e6, "mean_ms": average,
		"bucket_width_ms": .1, "overflow_ge_ms": 10000, "overflow_count": h.buckets[histogramBuckets-1]}
}

type config struct {
	address, path, expected, runID string
	duration                       time.Duration
	concurrency, queue, maxBody    int
	rate                           float64
	timeout                        time.Duration
	keepAliveIdle                  time.Duration
}

type job struct {
	id        uint64
	scheduled time.Time
}

type outcome struct {
	category, stage, detail, failureKind string
	status                               int
	bytesWritten                         uint64
	writeCompleted                       bool
	responseBytes                        int
	responseHeaderBytes                  int
	responseHeaderFramingComplete        bool
}

type ledger struct {
	mu                                        sync.Mutex
	planned, started, completed               uint64
	notStartedQueue, notStartedCutoff         uint64
	notStartedSchedulerCutoff                 uint64
	active, peakActive, peakQueue             uint64
	dials, connections, peakConnections       uint64
	bytesWritten, responseBytes               uint64
	writeCompleted, writePartial, writeZero   uint64
	counts                                    map[string]uint64
	statuses                                  map[string]uint64
	stages                                    map[string]uint64
	failureKinds                              map[string]uint64
	errorSamplesByCategory                    map[string]uint64
	errorsObservedBySampleCategory            map[string]uint64
	samples                                   []map[string]any
	latency, successfulLatency, queueLatency  histogram
	endToEndLatency, schedulerLag             histogram
	firstStarted, lastStarted, lastCompletion time.Time
	measurementDeadline                       time.Time
	completedInWindow, correctInWindow        uint64
}

func newLedger() *ledger {
	return &ledger{counts: map[string]uint64{"correct_success": 0, "http_rejected": 0,
		"other_http_failure": 0, "transport_or_timeout_failure": 0, "unfinished_at_cutoff": 0},
		statuses: make(map[string]uint64), stages: make(map[string]uint64), failureKinds: make(map[string]uint64),
		errorSamplesByCategory: make(map[string]uint64), errorsObservedBySampleCategory: make(map[string]uint64)}
}

func (l *ledger) start(j job, now time.Time) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.started++
	l.active++
	if l.active > l.peakActive {
		l.peakActive = l.active
	}
	if l.firstStarted.IsZero() {
		l.firstStarted = now
	}
	l.lastStarted = now
	l.queueLatency.add(now.Sub(j.scheduled))
}

func (l *ledger) finish(j job, began time.Time, result outcome) {
	now := time.Now()
	l.mu.Lock()
	defer l.mu.Unlock()
	l.active--
	l.completed++
	l.counts[result.category]++
	l.latency.add(now.Sub(began))
	l.endToEndLatency.add(now.Sub(j.scheduled))
	if result.category == "correct_success" {
		l.successfulLatency.add(now.Sub(began))
	}
	if result.status > 0 {
		l.statuses[fmt.Sprint(result.status)]++
	}
	if result.stage != "" {
		l.stages[result.stage]++
	}
	if result.failureKind != "" {
		l.failureKinds[result.failureKind]++
	}
	l.bytesWritten += result.bytesWritten
	l.responseBytes += uint64(result.responseBytes)
	if result.writeCompleted {
		l.writeCompleted++
	} else if result.bytesWritten > 0 {
		l.writePartial++
	} else {
		l.writeZero++
	}
	l.lastCompletion = now
	if !now.After(l.measurementDeadline) {
		l.completedInWindow++
		if result.category == "correct_success" {
			l.correctInWindow++
		}
	}
	if result.category != "correct_success" {
		sampleCategory := result.category
		if result.failureKind != "" {
			sampleCategory += "/" + result.failureKind
		}
		l.errorsObservedBySampleCategory[sampleCategory]++
		if len(l.samples) < errorSampleLimit && l.errorSamplesByCategory[sampleCategory] < errorSamplesPerCategory {
			l.errorSamplesByCategory[sampleCategory]++
			l.samples = append(l.samples, map[string]any{"request_id": j.id, "category": result.category,
				"sample_category": sampleCategory, "status": result.status, "stage": result.stage,
				"failure_kind": result.failureKind, "detail": result.detail,
				"request_bytes_written": result.bytesWritten, "request_write_completed": result.writeCompleted,
				"response_header_bytes_read":       result.responseHeaderBytes,
				"response_header_framing_complete": result.responseHeaderFramingComplete,
				"elapsed_ms":                       float64(now.Sub(began)) / 1e6})
		}
	}
}

type countedConn struct {
	net.Conn
	written uint64
}

func (c *countedConn) Write(p []byte) (int, error) {
	n, err := c.Conn.Write(p)
	c.written += uint64(n)
	return n, err
}

type worker struct {
	config   config
	ledger   *ledger
	conn     *countedConn
	reader   *bufio.Reader
	lastUsed time.Time
}

func (w *worker) close() {
	if w.conn == nil {
		return
	}
	_ = w.conn.Close()
	w.conn, w.reader = nil, nil
	w.ledger.mu.Lock()
	w.ledger.connections--
	w.ledger.mu.Unlock()
}

func boundedText(s string) string {
	if len(s) > 256 {
		return s[:256] + " [truncated]"
	}
	return s
}

func readResponse(reader *bufio.Reader, request *http.Request) (*http.Response, int, bool, error) {
	// ReadResponse does not itself impose a total MIME header bound. Preserve
	// its parser while bounding that input, including arbitrarily long lines.
	header := make([]byte, 0, 1024)
	lineStart := 0
	for {
		part, err := reader.ReadSlice('\n')
		if len(header)+len(part) > maxResponseHeaderBytes {
			return nil, len(header) + len(part), false, fmt.Errorf("response header exceeds %d bytes", maxResponseHeaderBytes)
		}
		header = append(header, part...)
		if err != nil {
			if errors.Is(err, bufio.ErrBufferFull) {
				continue
			}
			return nil, len(header), false, err
		}
		line := header[lineStart:]
		if bytes.Equal(line, []byte("\r\n")) || bytes.Equal(line, []byte("\n")) {
			break
		}
		lineStart = len(header)
	}
	response, err := http.ReadResponse(bufio.NewReaderSize(io.MultiReader(bytes.NewReader(header), reader), 8192), request)
	return response, len(header), true, err
}

func (w *worker) request(j job, began time.Time) (result outcome) {
	result.category = "transport_or_timeout_failure"
	// Retire an idle connection before a new distinct attempt, matching the
	// existing fixture client's policy. Never replay an attempt that failed.
	if w.conn != nil && w.config.keepAliveIdle > 0 && began.Sub(w.lastUsed) >= w.config.keepAliveIdle {
		w.close()
	}
	failAt := func(stage string, err error) {
		result.stage, result.detail = stage, boundedText(err.Error())
		var networkError net.Error
		switch {
		case errors.As(err, &networkError) && networkError.Timeout():
			result.failureKind = "timeout"
		case errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF):
			result.failureKind = "eof"
		default:
			result.failureKind = "other_transport_or_protocol"
		}
	}
	var before uint64
	defer func() {
		if w.conn != nil {
			result.bytesWritten = w.conn.written - before
		}
		if result.category == "transport_or_timeout_failure" || result.stage == "body_limit" {
			w.close()
		} else if w.conn != nil {
			w.lastUsed = time.Now()
		}
	}()
	deadline := began.Add(w.config.timeout)
	if w.conn == nil {
		w.ledger.mu.Lock()
		w.ledger.dials++
		w.ledger.mu.Unlock()
		ctx, cancel := context.WithDeadline(context.Background(), deadline)
		connection, err := (&net.Dialer{Timeout: w.config.timeout, KeepAlive: 30 * time.Second, FallbackDelay: -1}).DialContext(ctx, "tcp", w.config.address)
		cancel()
		if err != nil {
			failAt("dial", err)
			return
		}
		w.conn = &countedConn{Conn: connection}
		w.reader = bufio.NewReaderSize(w.conn, 8192)
		w.ledger.mu.Lock()
		w.ledger.connections++
		if w.ledger.connections > w.ledger.peakConnections {
			w.ledger.peakConnections = w.ledger.connections
		}
		w.ledger.mu.Unlock()
	}
	before = w.conn.written
	if err := w.conn.SetDeadline(deadline); err != nil {
		failAt("set_deadline", err)
		return
	}
	request := &http.Request{Method: "GET", URL: &url.URL{Opaque: w.config.path},
		Host: w.config.address, Proto: "HTTP/1.1", ProtoMajor: 1, ProtoMinor: 1,
		Header: http.Header{"Connection": {"keep-alive"}, "Accept-Encoding": {"identity"},
			"X-Ku-Test-Run": {w.config.runID}, "X-Ku-Test-Request": {fmt.Sprint(j.id)}}}
	if err := request.Write(w.conn); err != nil {
		failAt("write", err)
		return
	}
	result.writeCompleted = true
	response, headerBytes, headerComplete, err := readResponse(w.reader, request)
	result.responseHeaderBytes, result.responseHeaderFramingComplete = headerBytes, headerComplete
	if err != nil {
		failAt("read_status", err)
		return
	}
	result.status = response.StatusCode
	body, readError := io.ReadAll(io.LimitReader(response.Body, int64(w.config.maxBody)+1))
	result.responseBytes = len(body)
	if len(body) > w.config.maxBody || readError != nil {
		// Close the transport first: Body.Close must never drain an unbounded
		// oversized or incomplete response outside this request's deadline.
		result.bytesWritten = w.conn.written - before
		w.close()
		_ = response.Body.Close()
		if readError != nil {
			failAt("read_body", readError)
		} else {
			result.category, result.stage, result.detail = "other_http_failure", "body_limit", "response exceeds max-response-bytes"
		}
		return
	}
	_ = response.Body.Close()
	result.bytesWritten = w.conn.written - before
	if response.Close || response.StatusCode < 200 {
		w.close()
	}
	switch {
	case response.StatusCode == 200 && string(body) == w.config.expected:
		result.category = "correct_success"
	case response.StatusCode == 429 || response.StatusCode == 503:
		result.category, result.detail = "http_rejected", boundedText(string(body))
	case response.StatusCode == 200:
		result.category, result.stage, result.detail = "other_http_failure", "wrong_body", boundedText(string(body))
	default:
		result.category, result.detail = "other_http_failure", boundedText(string(body))
	}
	return
}

type resources struct {
	mu                                         sync.Mutex
	baseline, final                            runtime.MemStats
	peakHeapAlloc, peakHeapSys, peakStackInuse uint64
	peakGoroutines                             int
	samples                                    uint64
}

func (r *resources) sample() {
	var m runtime.MemStats
	runtime.ReadMemStats(&m)
	g := runtime.NumGoroutine()
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.samples == 0 {
		r.baseline = m
	}
	r.final = m
	r.samples++
	if m.HeapAlloc > r.peakHeapAlloc {
		r.peakHeapAlloc = m.HeapAlloc
	}
	if m.HeapSys > r.peakHeapSys {
		r.peakHeapSys = m.HeapSys
	}
	if m.StackInuse > r.peakStackInuse {
		r.peakStackInuse = m.StackInuse
	}
	if g > r.peakGoroutines {
		r.peakGoroutines = g
	}
}

func fail(message string) {
	fmt.Fprintln(os.Stderr, message)
	os.Exit(2)
}

func parseConfig() config {
	var c config
	var seconds float64
	var timeoutMS int
	var keepAliveIdleMS int
	flag.StringVar(&c.address, "address", "", "HTTP host:port (no TLS or proxy)")
	flag.StringVar(&c.path, "path", "/cpu", "origin-form URL path including optional query")
	flag.StringVar(&c.expected, "expected", "cpu-ready", "exact successful response body")
	flag.StringVar(&c.runID, "run-id", "capacity", "bounded diagnostic run identifier")
	flag.Float64Var(&seconds, "duration", 30, "measurement seconds, 0 < duration <= 3600")
	flag.IntVar(&c.concurrency, "concurrency", 64, "fixed workers / maximum live connections (1..4096)")
	flag.IntVar(&c.queue, "queue", 64, "open-mode pending descriptor limit (0..65536)")
	flag.Float64Var(&c.rate, "rate", 0, "0: continuously supplied closed loop; >0: independently scheduled arrivals/sec")
	flag.IntVar(&timeoutMS, "timeout-ms", 3000, "absolute per-request dial/write/read timeout (1..60000)")
	flag.IntVar(&keepAliveIdleMS, "keepalive-idle-ms", 500, "retire idle connections before a new request (0 disables, max 60000)")
	flag.IntVar(&c.maxBody, "max-response-bytes", 65536, "response allocation bound (1..1048576)")
	flag.Parse()
	if _, _, err := net.SplitHostPort(c.address); err != nil {
		fail("--address must be host:port: " + err.Error())
	}
	parsed, err := url.ParseRequestURI(c.path)
	if err != nil || !strings.HasPrefix(c.path, "/") || parsed.IsAbs() || strings.ContainsAny(c.path, "\r\n") {
		fail("--path must be an origin-form HTTP URL without CR/LF")
	}
	if math.IsNaN(seconds) || math.IsInf(seconds, 0) || seconds <= 0 || seconds > 3600 ||
		math.IsNaN(c.rate) || math.IsInf(c.rate, 0) || c.rate < 0 || c.rate > 1000000 ||
		seconds*c.rate > 10000000 || c.concurrency < 1 || c.concurrency > 4096 ||
		c.queue < 0 || c.queue > 65536 || timeoutMS < 1 || timeoutMS > 60000 || keepAliveIdleMS < 0 || keepAliveIdleMS > 60000 ||
		c.maxBody < 1 || c.maxBody > 1048576 || len(c.expected) > c.maxBody {
		fail("invalid bounded duration/rate/concurrency/queue/timeout/response limit (open total <= 10000000)")
	}
	if len(c.runID) == 0 || len(c.runID) > 96 || strings.ContainsAny(c.runID, "\r\n") {
		fail("--run-id must be 1..96 bytes without CR/LF")
	}
	c.duration = time.Duration(seconds * float64(time.Second))
	if c.duration < time.Microsecond {
		fail("--duration must be at least one microsecond")
	}
	c.timeout = time.Duration(timeoutMS) * time.Millisecond
	c.keepAliveIdle = time.Duration(keepAliveIdleMS) * time.Millisecond
	return c
}

func run(c config) map[string]any {
	l := newLedger()
	var resource resources
	resource.sample()
	sampleStop := make(chan struct{})
	sampleDone := make(chan struct{})
	go func() {
		defer close(sampleDone)
		ticker := time.NewTicker(250 * time.Millisecond)
		defer ticker.Stop()
		for {
			select {
			case <-ticker.C:
				resource.sample()
			case <-sampleStop:
				return
			}
		}
	}()
	startGate := make(chan struct{})
	work := make(chan job, c.queue)
	var wg sync.WaitGroup
	var began, deadline time.Time
	for index := 0; index < c.concurrency; index++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			w := worker{config: c, ledger: l}
			defer w.close()
			<-startGate
			for {
				var j job
				if c.rate > 0 {
					var open bool
					j, open = <-work
					if !open {
						return
					}
				} else {
					now := time.Now()
					if !now.Before(deadline) {
						return
					}
					l.mu.Lock()
					l.planned++
					j = job{id: l.planned, scheduled: now}
					l.mu.Unlock()
				}
				now := time.Now()
				if !now.Before(deadline) {
					l.mu.Lock()
					l.notStartedCutoff++
					l.mu.Unlock()
					continue
				}
				l.start(j, now)
				l.finish(j, now, w.request(j, now))
			}
		}()
	}
	began = time.Now()
	deadline = began.Add(c.duration)
	l.measurementDeadline = deadline
	close(startGate)
	schedulerDone := make(chan struct{})
	if c.rate > 0 {
		go func() {
			defer close(schedulerDone)
			defer close(work)
			planned := uint64(math.Ceil(c.duration.Seconds() * c.rate))
			l.mu.Lock()
			l.planned = planned
			l.mu.Unlock()
			timer := time.NewTimer(time.Hour)
			if !timer.Stop() {
				<-timer.C
			}
			defer timer.Stop()
			for index := uint64(0); index < planned; index++ {
				target := began.Add(time.Duration(float64(index) / c.rate * float64(time.Second)))
				if delay := time.Until(target); delay > 0 {
					timer.Reset(delay)
					<-timer.C
				}
				now := time.Now()
				if !now.Before(deadline) {
					l.mu.Lock()
					l.notStartedSchedulerCutoff += planned - index
					l.mu.Unlock()
					return
				}
				l.mu.Lock()
				l.schedulerLag.add(now.Sub(target))
				l.mu.Unlock()
				select {
				case work <- job{id: index + 1, scheduled: target}:
					l.mu.Lock()
					if depth := uint64(len(work)); depth > l.peakQueue {
						l.peakQueue = depth
					}
					l.mu.Unlock()
				default:
					l.mu.Lock()
					l.notStartedQueue++
					l.mu.Unlock()
				}
			}
		}()
	} else {
		close(schedulerDone)
	}
	time.Sleep(time.Until(deadline))
	l.mu.Lock()
	cutoff := map[string]any{"observed_seconds": time.Since(began).Seconds(), "started": l.started,
		"completed": l.completed, "inflight": l.active}
	l.mu.Unlock()
	<-schedulerDone
	wg.Wait() // Each already-started operation has one absolute, finite deadline.
	finished := time.Now()
	close(sampleStop)
	<-sampleDone
	resource.sample()
	l.mu.Lock()
	defer l.mu.Unlock()
	notStarted := l.notStartedQueue + l.notStartedCutoff + l.notStartedSchedulerCutoff
	var classified uint64
	for _, count := range l.counts {
		classified += count
	}
	mode := "closed"
	if c.rate > 0 {
		mode = "open"
	}
	secondsFromStart := func(t time.Time) any {
		if t.IsZero() {
			return nil
		}
		return t.Sub(began).Seconds()
	}
	return map[string]any{
		"status": "completed", "schema": "ku-capacity-client-v1", "run_id": c.runID,
		"configuration": map[string]any{"address": c.address, "path": c.path, "expected_bytes": len(c.expected),
			"mode": mode, "duration_seconds": c.duration.Seconds(), "target_rps": c.rate,
			"workers": c.concurrency, "maximum_live_connections": c.concurrency,
			"queue_descriptors_limit": c.queue, "per_request_timeout_ms": c.timeout.Milliseconds(),
			"keepalive_idle_retirement_ms": c.keepAliveIdle.Milliseconds(),
			"max_response_bytes":           c.maxBody, "max_response_header_bytes": maxResponseHeaderBytes,
			"keep_alive": true, "pipelining": false,
			"automatic_retries": false, "redirects": false, "compression": false},
		"timing": map[string]any{"measurement_seconds": c.duration.Seconds(), "with_cleanup_seconds": finished.Sub(began).Seconds(),
			"first_started_seconds": secondsFromStart(l.firstStarted), "last_started_seconds": secondsFromStart(l.lastStarted),
			"last_completion_seconds": secondsFromStart(l.lastCompletion), "cutoff_snapshot": cutoff},
		"ledger": map[string]any{"planned": l.planned, "not_started": notStarted, "started": l.started,
			"not_started_reasons": map[string]uint64{"client_queue_full": l.notStartedQueue,
				"queued_or_closed_at_cutoff": l.notStartedCutoff, "scheduler_at_cutoff": l.notStartedSchedulerCutoff},
			"completed": l.completed, "completed_in_measurement_window": l.completedInWindow,
			"correct_in_measurement_window": l.correctInWindow, "results": l.counts, "http_statuses": l.statuses,
			"failure_stages": l.stages, "failure_kinds": l.failureKinds, "active_after_cleanup": l.active,
			"planned_reconciled":      l.planned == notStarted+l.started,
			"started_reconciled":      l.started == classified && l.started == l.completed,
			"request_write_completed": l.writeCompleted, "request_write_partial": l.writePartial,
			"no_request_bytes_written": l.writeZero, "request_bytes_written": l.bytesWritten,
			"response_bytes_read": l.responseBytes,
			"server_received":     nil, "server_received_note": "Collect before/after server complete-request counters separately; connect/write success does not prove server reception."},
		"rates": map[string]any{"started_per_measurement_second": float64(l.started) / c.duration.Seconds(),
			"correct_per_measurement_second":       float64(l.correctInWindow) / c.duration.Seconds(),
			"correct_per_second_including_cleanup": float64(l.counts["correct_success"]) / finished.Sub(began).Seconds()},
		"latency": map[string]any{"start_to_terminal": l.latency.value(), "correct_start_to_terminal": l.successfulLatency.value(),
			"schedule_to_start": l.queueLatency.value(), "schedule_to_terminal": l.endToEndLatency.value(),
			"scheduler_lag": l.schedulerLag.value(),
			"scope":         "start_to_terminal includes dial/write/read; schedule_to_start includes open scheduler and client queue delay; final outcomes include bounded cleanup, without replay."},
		"connections": map[string]any{"dial_attempts": l.dials, "peak_live": l.peakConnections,
			"live_after_cleanup": l.connections, "peak_inflight": l.peakActive,
			"observed_peak_queue_descriptors": l.peakQueue},
		"client_resources": map[string]any{"pid": os.Getpid(), "go_version": runtime.Version(),
			"os": runtime.GOOS, "arch": runtime.GOARCH, "logical_cpus": runtime.NumCPU(), "gomaxprocs": runtime.GOMAXPROCS(0),
			"sample_interval_ms": 250, "samples": resource.samples, "heap_alloc_baseline_bytes": resource.baseline.HeapAlloc,
			"heap_alloc_final_bytes": resource.final.HeapAlloc, "heap_alloc_peak_bytes": resource.peakHeapAlloc,
			"heap_sys_peak_bytes": resource.peakHeapSys, "stack_inuse_peak_bytes": resource.peakStackInuse,
			"goroutines_peak": resource.peakGoroutines,
			"cpu_seconds":     nil, "cpu_note": "External process CPU sampling is required; Go heap counters are not RSS or total process memory."},
		"first_errors": l.samples,
		"error_sampling": map[string]any{"total_limit": errorSampleLimit, "per_sample_category_limit": errorSamplesPerCategory,
			"retained_by_sample_category": l.errorSamplesByCategory, "observed_by_sample_category": l.errorsObservedBySampleCategory,
			"read_status_scope": "Dial/write already succeeded; read_status covers reading and parsing the complete response headers, not just the status line or first byte. response_header_bytes_read counts header bytes consumed before that result."},
		"scope": "One planned operation is one GET attempt. HTTP 429/503 are rejection; other status or wrong body is other_http_failure. A request still running at cutoff is observed separately and classified exactly once after finite cleanup. No server capacity claim is inferred from planned or written counts.",
	}
}

func main() {
	c := parseConfig()
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(run(c)); err != nil && !errors.Is(err, io.ErrClosedPipe) {
		fail(err.Error())
	}
}
