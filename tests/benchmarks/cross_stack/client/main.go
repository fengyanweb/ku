// Cross-stack open-arrival client. The raw TCP/header parser, deadline and
// finite arrival ledger derive from tests/acceptance/capacity_client.go:
// SHA-256 4c8877f6435cee1f330eca009dd572b6f902238edd32e3b04e4f9a7d49eee857.
// There is no http.Client/Transport and no request replay on any error.
package main

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"encoding/csv"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math"
	"math/bits"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"
)

const maxHeader = 32768
const maxBody = 65536

// Fixed cells cover every nonnegative int64 duration without dropping counts.
const maxHistogramKeys = 256 + 43*128
const maxEvents = 1073741824
const histogramNS = int64(10000) // 10 microseconds; quantiles are upper bounds.

type endpoint struct {
	Method string `json:"method"`
	Path   string `json:"path"`
	Work   []int  `json:"work_units"`
	Seeds  []int  `json:"seed_lengths"`
}
type corpusSpec struct {
	Path, SHA256 string
	Counter      int64
}
type protocol struct {
	Schema    int
	Endpoints map[string]endpoint
	Corpora   map[string]corpusSpec
	Database  struct{ Rows int }
	SLO       map[string]json.RawMessage
}
type corpus struct {
	bytes   []byte
	data    any
	counter int64
	hash    string
}
type config struct {
	address, workload, protocolPath, corporaDir, corpusPath, output, events string
	duration, timeout, idle                                                 time.Duration
	rate                                                                    float64
	cpuRate, taskMixShortSLOMS                                              float64
	cpuCancelEvery                                                          uint64
	cpuCancelDelay                                                          time.Duration
	concurrency, queue                                                      int
	prefix                                                                  uint64
	protocol                                                                protocol
	protocolHash                                                            string
	corpora                                                                 map[string]corpus
	checksums                                                               map[string]string
}

type descriptorPreparation struct {
	supported                                         bool
	method                                            string
	configuredConnections                             int
	maxOpenFD, targetFD, softLimit, runtimeReserveFDs uint64
	limitClamped, duplicateClosed, sourceClosed       bool
	duplicateFD                                       int
	started, finished                                 time.Time
}

func (p descriptorPreparation) report(origin time.Time, anchor monotonicAnchor) map[string]any {
	return map[string]any{
		"supported":                       p.supported,
		"method":                          p.method,
		"configured_max_live_connections": p.configuredConnections,
		"max_open_fd_before":              p.maxOpenFD,
		"target_fd":                       p.targetFD,
		"soft_limit":                      p.softLimit,
		"runtime_reserve_fds":             p.runtimeReserveFDs,
		"limit_clamped":                   p.limitClamped,
		"duplicate_fd":                    p.duplicateFD,
		"dup_closed":                      p.duplicateClosed,
		"source_closed":                   p.sourceClosed,
		"elapsed_ns":                      p.finished.Sub(p.started).Nanoseconds(),
		"started_offset_ns":               p.started.Sub(origin).Nanoseconds(),
		"completed_offset_ns":             p.finished.Sub(origin).Nanoseconds(),
		"started_monotonic_ns":            anchor.at(p.started.Sub(origin).Nanoseconds()),
		"completed_monotonic_ns":          anchor.at(p.finished.Sub(origin).Nanoseconds()),
		"complete_before_origin":          !p.finished.After(origin),
		"network_prewarm":                 false,
		"preparation_accounting":          "one-time driver startup before workers and arrival origin; no requests, socket prewarm, rlimit change or startup CPU subtraction; unsupported platforms explicitly no-op",
	}
}

type job struct {
	index       uint64
	streamIndex uint64
	cpuCancel   bool
	kind, rid   string
	scheduled   time.Time
	generated   time.Time // Arrival dispatcher, before client queue/dial/write.
}
type result struct {
	category, stage, detail               string
	reused                                bool
	dialTime                              time.Duration
	status                                int
	sent, completed                       time.Time
	bytesWritten, bytesRead               int64
	writeComplete, responseComplete, late bool
	cancelDue, cancelIssued               time.Time
}
type histogram struct {
	bins  []uint64
	count uint64
	maxNS int64
	sum   float64
}
type histogramBudget struct {
	keys     int
	overflow bool
}

// Linear 10us cells below 2.56ms; 128 cells per doubling afterwards.
// The conservative upper edge has <=1/128 relative width above 2.56ms.
// A maximum-duration window allocates at most (3600+8*9)*5760*8 bytes
// of cells, independent of request count (normally only 30 seconds).
func histogramIndex(n int64) int {
	q := max(n, 0) / histogramNS
	if q < 256 {
		return int(q)
	}
	shift := bits.Len64(uint64(q)) - 8
	return 256 + (shift-1)*128 + int((q-(int64(1)<<(shift+7)))>>shift)
}
func histogramUpper(index int) int64 {
	if index < 256 {
		return int64(index+1) * histogramNS
	}
	shift, offset := (index-256)/128+1, (index-256)%128
	units := uint64(129+offset) << shift
	if units > uint64(math.MaxInt64/histogramNS) {
		return math.MaxInt64
	}
	return int64(units) * histogramNS
}
func (h *histogram) add(d time.Duration, b *histogramBudget) {
	n := max(int64(d), 0)
	bucket := histogramIndex(n)
	if h.bins == nil {
		h.bins = make([]uint64, maxHistogramKeys)
	}
	if h.bins[bucket] == 0 {
		b.keys++
	}
	h.bins[bucket]++
	h.count++
	h.sum += float64(n)
	h.maxNS = max(h.maxNS, n)
}
func (h *histogram) value() map[string]any {
	v := map[string]any{"count": h.count, "minimum_resolution_us": 10, "maximum_relative_bucket_width": 1.0 / 128, "max_ms": float64(h.maxNS) / 1e6, "representation": "v5-fixed-log128", "quantiles": "conservative upper bucket edge; never averaged across rounds"}
	for _, q := range []struct {
		name     string
		fraction float64
		minimum  uint64
	}{{"p50_ms", .5, 1}, {"p95_ms", .95, 20}, {"p99_ms", .99, 100}, {"p999_ms", .999, 1000}} {
		v[q.name] = nil
		if h.count < q.minimum {
			continue
		}
		target := uint64(math.Ceil(float64(h.count) * q.fraction))
		var total uint64
		for k, count := range h.bins {
			total += count
			if total >= target {
				v[q.name] = float64(histogramUpper(k)) / 1e6
				break
			}
		}
	}
	if h.count > 0 {
		v["mean_ms"] = h.sum / float64(h.count) / 1e6
	}
	return v
}

type stats struct {
	Planned                                           uint64            `json:"planned"`
	Attempted                                         uint64            `json:"attempted"`
	Sent                                              uint64            `json:"sent"`
	Results                                           map[string]uint64 `json:"results"`
	CorrectWindow                                     uint64            `json:"correct_in_window"`
	Late                                              uint64            `json:"late_response_events"`
	SLOFailed                                         uint64            `json:"correct_over_slo"`
	Written                                           int64             `json:"request_bytes_written"`
	Read                                              int64             `json:"response_bytes_read"`
	correctScheduled, correctSent, allScheduled, lag  histogram
	queueDelay, dialDelay, coldLatency, reusedLatency histogram
	generationLag                                     histogram
	transportCounts                                   map[string]uint64
	sentErrors                                        uint64
	http500                                           uint64
	plannedCancels, cancelSent, cancelIssued          uint64
	normalCorrect, normalSentErrors, timelyWindow     uint64
	cancelResults                                     map[string]uint64
}

func newStats() *stats {
	return &stats{Results: map[string]uint64{}, transportCounts: map[string]uint64{}}
}
func incrementTransport(counts map[string]uint64, key string) {
	if _, known := counts[key]; known || len(counts) < 128 {
		counts[key]++
	} else {
		counts["other_transport_key"]++
	}
}
func (s *stats) view(duration float64) map[string]any {
	terminal := uint64(0)
	for _, n := range s.Results {
		terminal += n
	}
	correct := s.Results["correct"]
	failurePlanned, failureSent := any(nil), any(nil)
	if s.Planned > 0 {
		failurePlanned = float64(s.Planned-correct) / float64(s.Planned)
	}
	if s.Sent > 0 {
		failureSent = float64(s.sentErrors) / float64(s.Sent)
	}
	return map[string]any{"planned": s.Planned, "attempted": s.Attempted, "sent": s.Sent, "terminal": terminal, "results": s.Results, "planned_reconciled": terminal == s.Planned, "correct_in_window": s.CorrectWindow, "goodput_rps": float64(s.CorrectWindow) / duration, "correct_total": correct, "failure_fraction_planned": failurePlanned, "failure_fraction_sent": failureSent, "correct_over_slo": s.SLOFailed, "http_500_total": s.http500, "late_response_events": s.Late, "request_bytes_written": s.Written, "response_bytes_read": s.Read, "correct_scheduled_to_complete": s.correctScheduled.value(), "correct_sent_to_complete": s.correctSent.value(), "all_scheduled_to_terminal": s.allScheduled.value(), "scheduled_to_generated": s.generationLag.value(), "scheduled_to_sent": s.lag.value(), "scheduled_to_attempt": s.queueDelay.value(), "dial_duration": s.dialDelay.value(), "cold_connection_scheduled_to_complete": s.coldLatency.value(), "reused_connection_scheduled_to_complete": s.reusedLatency.value(), "transport_counts": s.transportCounts}
}

func (s *stats) taskMixView(duration, slo float64) map[string]any {
	v := s.view(duration)
	normalPlanned, normalSent := s.Planned-s.plannedCancels, s.Sent-s.cancelSent
	v["normal_planned"] = normalPlanned
	v["normal_sent"] = normalSent
	v["normal_correct"] = s.normalCorrect
	v["normal_failure_fraction_planned"] = nil
	v["normal_failure_fraction_sent"] = nil
	if normalPlanned > 0 {
		v["normal_failure_fraction_planned"] = float64(normalPlanned-s.normalCorrect) / float64(normalPlanned)
	}
	if normalSent > 0 {
		v["normal_failure_fraction_sent"] = float64(s.normalSentErrors) / float64(normalSent)
	}
	v["SLO_ms"] = slo
	v["correct_within_SLO_in_window"] = s.timelyWindow
	v["SLO_goodput_rps"] = float64(s.timelyWindow) / duration
	cancelResults := s.cancelResults
	if cancelResults == nil {
		cancelResults = map[string]uint64{}
	}
	v["planned_cancellation"] = map[string]any{"planned": s.plannedCancels, "sent": s.cancelSent, "reset_issued": s.cancelIssued, "results": cancelResults, "scope": "planned reset is not correctness, server-stop, cleanup or resource-reuse proof"}
	return v
}

type secondStats struct {
	planned, attempted, sent, correct uint64
	results                           map[string]uint64
	latency                           histogram
	kinds                             map[string]map[string]uint64
	completedCorrect                  uint64
	completedKinds                    map[string]uint64
}

// Completion history, producer counts/type registration, and resources have
// separate lock domains. Only finish nests locks: mu -> planMu or resourceMu,
// never the reverse. Resource sections contain scalar updates/copies only.
type ledger struct {
	planMu                                                            sync.Mutex
	resourceMu                                                        sync.Mutex
	mu                                                                sync.Mutex
	all                                                               *stats
	types                                                             map[string]*stats
	seconds                                                           []secondStats
	budget                                                            histogramBudget
	origin, cutoff                                                    time.Time
	lastActualSent, lastActualCompleted, lastCompleteResponse         time.Time
	lastActualSentRID, lastActualCompletedRID                         string
	lastTerminal                                                      time.Time
	lastTerminalRID                                                   string
	active, peakActive, live, peakLive, dials, peakQueue, poolExpired uint64
	samples                                                           []map[string]any
	cancelSamples                                                     []map[string]any
	events                                                            *eventWriter
}

type ledgerResourceSnapshot struct {
	active, peakActive, live, peakLive, dials, peakQueue, poolExpired uint64
}

func (l *ledger) resourceSnapshot() ledgerResourceSnapshot {
	l.resourceMu.Lock()
	defer l.resourceMu.Unlock()
	return ledgerResourceSnapshot{l.active, l.peakActive, l.live, l.peakLive, l.dials, l.peakQueue, l.poolExpired}
}

func (l *ledger) typed(kind string) *stats {
	l.planMu.Lock()
	defer l.planMu.Unlock()
	return l.typedLocked(kind)
}

// Caller holds planMu. Completion fields in the returned stats still require mu.
func (l *ledger) typedLocked(kind string) *stats {
	s := l.types[kind]
	if s == nil {
		s = newStats()
		l.types[kind] = s
	}
	return s
}
func (l *ledger) plan(j job) {
	l.planMu.Lock()
	defer l.planMu.Unlock()
	l.all.Planned++
	kind := l.typedLocked(j.kind)
	kind.Planned++
	if j.cpuCancel {
		l.all.plannedCancels++
		kind.plannedCancels++
	}
	i := int(j.scheduled.Sub(l.origin) / time.Second)
	if i >= len(l.seconds) {
		i = len(l.seconds) - 1
	}
	l.seconds[i].planned++
}
func (l *ledger) start() {
	l.resourceMu.Lock()
	l.active++
	l.peakActive = max(l.peakActive, l.active)
	l.resourceMu.Unlock()
}
func sloMS(kind string, c config) float64 {
	key := "json_p99_ms"
	switch kind {
	case "ok":
		if c.workload == "task_mix" {
			return c.taskMixShortSLOMS
		}
		key = "ok_p99_ms"
	case "cpu":
		key = "cpu_p99_ms"
	case "db":
		key = "db_p99_ms"
	case "timer":
		key = "timer_p99_ms"
	case "wait", "tcp":
		return 50000
	}
	var v float64
	_ = json.Unmarshal(c.protocol.SLO[key], &v)
	return v
}
func (l *ledger) finish(j job, began time.Time, r result, c config) {
	if r.completed.IsZero() {
		r.completed = time.Now()
	}
	attempted := !began.IsZero()
	sent := !r.sent.IsZero()
	l.mu.Lock()
	defer l.mu.Unlock()
	if attempted {
		l.resourceMu.Lock()
		l.active--
		l.resourceMu.Unlock()
	}
	if r.completed.After(l.lastTerminal) {
		l.lastTerminal, l.lastTerminalRID = r.completed, j.rid
	}
	if sent {
		if r.sent.After(l.lastActualSent) {
			l.lastActualSent, l.lastActualSentRID = r.sent, j.rid
		}
		if r.completed.After(l.lastActualCompleted) {
			l.lastActualCompleted, l.lastActualCompletedRID = r.completed, j.rid
		}
		if r.responseComplete && r.completed.After(l.lastCompleteResponse) {
			l.lastCompleteResponse = r.completed
		}
	}
	for _, s := range []*stats{l.all, l.typed(j.kind)} {
		s.Results[r.category]++
		if !j.generated.IsZero() {
			s.generationLag.add(j.generated.Sub(j.scheduled), &l.budget)
		}
		if j.cpuCancel {
			if s.cancelResults == nil {
				s.cancelResults = map[string]uint64{}
			}
			s.cancelResults[r.category]++
			if sent {
				s.cancelSent++
			}
			if !r.cancelIssued.IsZero() {
				s.cancelIssued++
			}
		}
		if r.status == 500 {
			s.http500++
		}
		if attempted {
			s.queueDelay.add(began.Sub(j.scheduled), &l.budget)
		}
		incrementTransport(s.transportCounts, "stage:"+r.stage)
		if r.status > 0 {
			incrementTransport(s.transportCounts, "status:"+strconv.Itoa(r.status))
		}
		if r.reused {
			s.transportCounts["reused_connection"]++
		} else if attempted {
			s.transportCounts["cold_connection"]++
		}
		if r.dialTime > 0 {
			s.dialDelay.add(r.dialTime, &l.budget)
		}
		if attempted {
			s.Attempted++
		}
		if sent {
			s.Sent++
			if r.category != "correct" {
				s.sentErrors++
				if c.workload == "task_mix" && !j.cpuCancel {
					s.normalSentErrors++
				}
			}
			s.lag.add(r.sent.Sub(j.scheduled), &l.budget)
		}
		if r.late {
			s.Late++
		}
		s.Written += r.bytesWritten
		s.Read += r.bytesRead
		s.allScheduled.add(r.completed.Sub(j.scheduled), &l.budget)
		if r.category == "correct" {
			if c.workload == "task_mix" && !j.cpuCancel {
				s.normalCorrect++
			}
			s.correctScheduled.add(r.completed.Sub(j.scheduled), &l.budget)
			if r.reused {
				s.reusedLatency.add(r.completed.Sub(j.scheduled), &l.budget)
			} else {
				s.coldLatency.add(r.completed.Sub(j.scheduled), &l.budget)
			}
			if sent {
				s.correctSent.add(r.completed.Sub(r.sent), &l.budget)
			}
			if !r.completed.After(l.cutoff) {
				s.CorrectWindow++
				if c.workload == "task_mix" && float64(r.completed.Sub(j.scheduled))/1e6 <= sloMS(j.kind, c) {
					s.timelyWindow++
				}
			}
			if float64(r.completed.Sub(j.scheduled))/1e6 > sloMS(j.kind, c) {
				s.SLOFailed++
			}
		}
	}
	i := min(int(j.scheduled.Sub(l.origin)/time.Second), len(l.seconds)-1)
	s := &l.seconds[i]
	if s.results == nil {
		s.results = map[string]uint64{}
		s.kinds = map[string]map[string]uint64{}
	}
	s.results[r.category]++
	if s.kinds[j.kind] == nil {
		s.kinds[j.kind] = map[string]uint64{}
	}
	s.kinds[j.kind][r.category]++
	if attempted {
		s.attempted++
	}
	if sent {
		s.sent++
	}
	if r.category == "correct" {
		s.latency.add(r.completed.Sub(j.scheduled), &l.budget)
		if !r.completed.After(l.cutoff) {
			s.correct++
			completionSecond := min(int(r.completed.Sub(l.origin)/time.Second), len(l.seconds)-1)
			completed := &l.seconds[completionSecond]
			completed.completedCorrect++
			if completed.completedKinds == nil {
				completed.completedKinds = map[string]uint64{}
			}
			completed.completedKinds[j.kind]++
		}
	}
	if r.category == "planned_reset" && len(l.cancelSamples) < 24 {
		l.cancelSamples = append(l.cancelSamples, map[string]any{"rid": j.rid, "cpu_sequence": j.streamIndex, "scheduled_ns": j.scheduled.Sub(l.origin).Nanoseconds(), "sent_ns": offset(r.sent, l.origin), "reset_due_ns": offset(r.cancelDue, l.origin), "reset_issued_ns": offset(r.cancelIssued, l.origin), "terminal_ns": r.completed.Sub(l.origin).Nanoseconds()})
	}
	if r.category != "correct" && r.category != "planned_reset" && len(l.samples) < 24 {
		sample := map[string]any{"rid": j.rid, "index": j.index, "type": j.kind, "terminal": r.category, "status": r.status, "stage": r.stage, "detail": truncate(r.detail), "reused_connection": r.reused, "scheduled_ns": j.scheduled.Sub(l.origin).Nanoseconds(), "sent_ns": offset(r.sent, l.origin), "completed_ns": r.completed.Sub(l.origin).Nanoseconds(), "bytes_written": r.bytesWritten, "bytes_read": r.bytesRead}
		if c.workload == "task_mix" {
			sample["stream_index"], sample["planned_cpu_cancel"] = j.streamIndex, j.cpuCancel
			sample["reset_due_ns"], sample["reset_issued_ns"] = offset(r.cancelDue, l.origin), offset(r.cancelIssued, l.origin)
		}
		l.samples = append(l.samples, sample)
	}
	if l.events != nil {
		l.events.offer(event{j: j, began: began, result: r, origin: l.origin})
	}
}
func offset(t, origin time.Time) any {
	if t.IsZero() {
		return nil
	}
	return t.Sub(origin).Nanoseconds()
}
func unixNano(t time.Time) any {
	if t.IsZero() {
		return nil
	}
	return t.UnixNano()
}
func truncate(s string) string {
	if len(s) > 256 {
		return s[:256]
	}
	return s
}

type event struct {
	j             job
	began, origin time.Time
	result        result
}
type eventWriter struct {
	queue   chan event
	done    chan struct{}
	dropped uint64
	written uint64
	err     string
	path    string
}

func openEvents(path string, taskMix ...bool) (*eventWriter, error) {
	f, e := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if e != nil {
		return nil, e
	}
	w := &eventWriter{queue: make(chan event, 8192), done: make(chan struct{}), path: path}
	go func() {
		defer close(w.done)
		z, _ := gzip.NewWriterLevel(f, gzip.BestSpeed)
		csvw := csv.NewWriter(z)
		header := []string{"rid", "type", "index", "scheduled_ns", "sent_ns", "completed_ns", "attempt_started_ns", "terminal", "status", "request_bytes_written", "response_bytes_read", "late_response", "reused_connection", "dial_duration_ns", "stage"}
		mixed := len(taskMix) > 0 && taskMix[0]
		if mixed {
			header = append(header, "stream_index", "planned_cpu_cancel", "reset_due_ns", "reset_issued_ns", "request_write_completed", "generated_ns")
		}
		err := csvw.Write(header)
		var total int64
		for row := range w.queue {
			if err != nil {
				continue
			}
			format := func(t time.Time) string {
				if t.IsZero() {
					return ""
				}
				return strconv.FormatInt(t.Sub(row.origin).Nanoseconds(), 10)
			}
			record := []string{row.j.rid, row.j.kind, strconv.FormatUint(row.j.index, 10), format(row.j.scheduled), format(row.result.sent), format(row.result.completed), format(row.began), row.result.category, strconv.Itoa(row.result.status), strconv.FormatInt(row.result.bytesWritten, 10), strconv.FormatInt(row.result.bytesRead, 10), strconv.FormatBool(row.result.late), strconv.FormatBool(row.result.reused), strconv.FormatInt(row.result.dialTime.Nanoseconds(), 10), row.result.stage}
			if mixed {
				record = append(record, strconv.FormatUint(row.j.streamIndex, 10), strconv.FormatBool(row.j.cpuCancel), format(row.result.cancelDue), format(row.result.cancelIssued), strconv.FormatBool(row.result.writeComplete), format(row.j.generated))
			}
			for _, v := range record {
				total += int64(len(v) + 1)
			}
			if total > maxEvents {
				err = fmt.Errorf("event CSV exceeds bounded uncompressed byte limit")
				continue
			}
			err = csvw.Write(record)
			if err == nil {
				w.written++
			}
		}
		csvw.Flush()
		if err == nil {
			err = csvw.Error()
		}
		if e := z.Close(); err == nil {
			err = e
		}
		if e := f.Close(); err == nil {
			err = e
		}
		if err != nil {
			w.err = err.Error()
		}
	}()
	return w, nil
}

// Called under the ledger mutex, so dropped has one writer at a time.
func (w *eventWriter) offer(e event) {
	select {
	case w.queue <- e:
	default:
		w.dropped++
	}
}
func (w *eventWriter) close() { close(w.queue); <-w.done }

type countedConn struct {
	net.Conn
	written int64
	first   time.Time
}

func (c *countedConn) Write(p []byte) (int, error) {
	now := time.Now()
	n, e := c.Conn.Write(p)
	if n > 0 {
		c.written += int64(n)
		if c.first.IsZero() {
			c.first = now
		}
	}
	return n, e
}

type idleConnection struct {
	conn   *countedConn
	reader *bufio.Reader
	last   time.Time
}

// Connections belong to one request while checked out. A successful request
// returns its connection to this bounded LIFO pool, independent of its worker.
// Expiry is checked before another distinct plan, with no background I/O.
type connectionPool struct {
	mu       sync.Mutex
	idle     []*idleConnection
	capacity int
	timeout  time.Duration
	l        *ledger
	closed   bool
}

func newConnectionPool(c config, l *ledger) *connectionPool {
	return &connectionPool{capacity: c.concurrency, timeout: c.idle, l: l}
}

func (p *connectionPool) discard(connection *idleConnection) error {
	err := connection.conn.Close()
	p.l.resourceMu.Lock()
	p.l.live--
	p.l.resourceMu.Unlock()
	return err
}

func (p *connectionPool) take() (*idleConnection, bool) {
	for {
		p.mu.Lock()
		if p.closed || len(p.idle) == 0 {
			closed := p.closed
			p.mu.Unlock()
			return nil, closed
		}
		index := len(p.idle) - 1
		connection := p.idle[index]
		p.idle[index] = nil
		p.idle = p.idle[:index]
		p.mu.Unlock()
		if p.timeout > 0 && time.Since(connection.last) >= p.timeout {
			p.l.resourceMu.Lock()
			p.l.poolExpired++
			p.l.resourceMu.Unlock()
			p.discard(connection)
			continue
		}
		return connection, false
	}
}

func (p *connectionPool) put(connection *idleConnection) {
	p.mu.Lock()
	if p.closed || len(p.idle) >= p.capacity {
		p.mu.Unlock()
		p.discard(connection)
		return
	}
	p.idle = append(p.idle, connection)
	p.mu.Unlock()
}

func (p *connectionPool) close() {
	p.mu.Lock()
	p.closed = true
	idle := p.idle
	p.idle = nil
	p.mu.Unlock()
	for _, connection := range idle {
		p.discard(connection)
	}
}

type worker struct {
	c    config
	l    *ledger
	pool *connectionPool
}

type workerPreparation struct {
	started, ready time.Time
	workers        int
}

// Construct every worker before establishing the arrival clock. The empty
// request queue is the only wait point: closing a separate start gate would
// make the entire pool runnable again at origin. beforeReady is an internal
// construction seam for the lifecycle regression; normal runs pass nil.
func prepareRequestWorkers(c config, l *ledger, pool *connectionPool, queue <-chan job, workers *sync.WaitGroup, beforeReady func(int)) workerPreparation {
	preparation := workerPreparation{started: time.Now(), workers: c.concurrency}
	var ready sync.WaitGroup
	ready.Add(c.concurrency)
	workers.Add(c.concurrency)
	for i := 0; i < c.concurrency; i++ {
		go func(index int) {
			defer workers.Done()
			w := worker{c: c, l: l, pool: pool}
			if beforeReady != nil {
				beforeReady(index)
			}
			ready.Done()
			for j := range queue {
				w.execute(j)
			}
		}(i)
	}
	ready.Wait()
	preparation.ready = time.Now()
	return preparation
}

func (w *worker) execute(j job) {
	now := time.Now()
	if !now.Before(j.scheduled.Add(w.c.timeout)) {
		w.l.finish(j, time.Time{}, result{category: "not_started_deadline", completed: now}, w.c)
		return
	}
	w.l.start()
	w.l.finish(j, now, w.request(j, now), w.c)
}

func enqueuePlanned(c config, l *ledger, j job, queue chan<- job, now time.Time) {
	// This existing timestamp distinguishes a late dispatcher from delay in
	// the client queue, connection acquisition, Write, or service backpressure.
	// It adds no timer, syscall or per-arrival ledger lock.
	if c.workload == "task_mix" {
		j.generated = now
	}
	// The cutoff defines the arrival plan and the throughput window, not a
	// replacement deadline. A delayed plan retains only its original budget.
	if !now.Before(j.scheduled.Add(c.timeout)) {
		l.finish(j, time.Time{}, result{category: "not_started_scheduler_deadline", completed: now}, c)
		return
	}
	select {
	case queue <- j:
		l.resourceMu.Lock()
		l.peakQueue = max(l.peakQueue, uint64(len(queue)))
		l.resourceMu.Unlock()
	default:
		l.finish(j, time.Time{}, result{category: "not_started_queue", completed: now}, c)
	}
}

func readResponse(reader *bufio.Reader, request *http.Request) (*http.Response, error) {
	header := make([]byte, 0, 1024)
	lineStart := 0
	for {
		part, e := reader.ReadSlice('\n')
		if len(header)+len(part) > maxHeader {
			return nil, fmt.Errorf("response header exceeds bound")
		}
		header = append(header, part...)
		if e != nil {
			if errors.Is(e, bufio.ErrBufferFull) {
				continue
			}
			return nil, e
		}
		line := header[lineStart:]
		if bytes.Equal(line, []byte("\r\n")) || bytes.Equal(line, []byte("\n")) {
			break
		}
		lineStart = len(header)
	}
	return http.ReadResponse(bufio.NewReaderSize(io.MultiReader(bytes.NewReader(header), reader), 8192), request)
}
func (w *worker) request(j job, began time.Time) (r result) {
	r.category = "connection_error"
	deadline := j.scheduled.Add(w.c.timeout)
	var connection *countedConn
	var lease *idleConnection
	var before int64
	discard := func() {
		if lease != nil {
			w.pool.discard(lease)
			lease = nil
		}
	}
	defer func() {
		r.completed = time.Now()
		if connection != nil {
			r.bytesWritten = connection.written - before
			r.sent = connection.first
		}
		if r.responseComplete && r.completed.After(deadline) {
			r.late = true
			// Preserve an actual wrong-body finding even when it also arrived late.
			if r.category != "body_error" {
				r.category = "timeout"
				r.stage = "completed_after_absolute_deadline"
			}
		}
		if !r.cancelIssued.IsZero() && r.completed.After(deadline) {
			r.category, r.stage = "timeout", "planned_reset_after_absolute_deadline"
		}
		if !r.responseComplete || r.category == "timeout" || r.category == "connection_error" || r.category == "protocol_error" || r.category == "body_error" {
			discard()
		} else if lease != nil {
			lease.last = r.completed
			w.pool.put(lease)
		}
	}()
	fail := func(stage string, e error) {
		r.stage = stage
		r.detail = e.Error()
		var n net.Error
		if errors.As(e, &n) && n.Timeout() {
			r.category = "timeout"
		} else if errors.Is(e, io.EOF) || errors.Is(e, io.ErrUnexpectedEOF) {
			r.category = "connection_error"
		} else if strings.Contains(stage, "parse") {
			r.category = "protocol_error"
		}
	}
	if !time.Now().Before(deadline) {
		r.category = "not_started_deadline"
		return
	}
	var closed bool
	lease, closed = w.pool.take()
	if closed {
		r.stage = "connection_pool_closed"
		return
	}
	// Pool expiry/locking and dialing spend the same original request budget.
	if !time.Now().Before(deadline) {
		r.category, r.stage = "timeout", "before_dial_deadline"
		return
	}
	r.reused = lease != nil
	if lease == nil {
		w.l.resourceMu.Lock()
		w.l.dials++
		w.l.resourceMu.Unlock()
		ctx, cancel := context.WithDeadline(context.Background(), deadline)
		dialStart := time.Now()
		conn, e := (&net.Dialer{FallbackDelay: -1}).DialContext(ctx, "tcp", w.c.address)
		r.dialTime = time.Since(dialStart)
		cancel()
		if e != nil {
			fail("dial", e)
			return
		}
		counted := &countedConn{Conn: conn}
		lease = &idleConnection{conn: counted, reader: bufio.NewReaderSize(counted, 8192)}
		w.l.resourceMu.Lock()
		w.l.live++
		w.l.peakLive = max(w.l.peakLive, w.l.live)
		w.l.resourceMu.Unlock()
	}
	connection = lease.conn
	before = connection.written
	connection.first = time.Time{}
	if e := connection.SetDeadline(deadline); e != nil {
		fail("deadline", e)
		return
	}
	method, path, body := requestSpec(w.c, j)
	headers := http.Header{"Connection": {"keep-alive"}, "Accept-Encoding": {"identity"}, "X-Cross-Stack-Request": {j.rid}, "X-Request-ID": {j.rid}}
	if body != nil {
		headers.Set("Content-Type", "application/json")
	}
	req := &http.Request{Method: method, URL: &url.URL{Opaque: path}, Host: w.c.address, Proto: "HTTP/1.1", ProtoMajor: 1, ProtoMinor: 1, Header: headers, ContentLength: int64(len(body))}
	if body != nil {
		req.Body = io.NopCloser(bytes.NewReader(body))
	}
	if !time.Now().Before(deadline) {
		r.category, r.stage = "timeout", "before_write_deadline"
		return
	}
	if e := req.Write(connection); e != nil {
		fail("write", e)
		return
	}
	r.writeComplete = true
	if j.cpuCancel {
		// One worker owns this socket. Wait after the complete write without a
		// second goroutine or a response read; RST does not prove server stop.
		r.cancelDue = connection.first.Add(w.c.cpuCancelDelay)
		if r.cancelDue.After(deadline) {
			r.category, r.stage = "timeout", "reset_due_after_absolute_deadline"
			return
		}
		if delay := time.Until(r.cancelDue); delay > 0 {
			time.Sleep(delay)
		}
		tcp, ok := connection.Conn.(*net.TCPConn)
		if !ok {
			r.category, r.stage = "cancel_issue_error", "reset_requires_TCP"
			return
		}
		if e := tcp.SetLinger(0); e != nil {
			r.category, r.stage, r.detail = "cancel_issue_error", "reset_linger", e.Error()
			return
		}
		issued := time.Now()
		e := w.pool.discard(lease)
		lease = nil // retired exactly once, including a Close failure
		if e != nil {
			r.category, r.stage, r.detail = "cancel_issue_error", "reset_close", e.Error()
			return
		}
		r.cancelIssued = issued
		r.category, r.stage = "planned_reset", "TCP_linger0_close"
		return
	}
	response, e := readResponse(lease.reader, req)
	if e != nil {
		fail("read_parse_header", e)
		return
	}
	r.status = response.StatusCode
	if encoding := response.Header.Get("Content-Encoding"); encoding != "" && encoding != "identity" {
		r.category = "protocol_error"
		r.stage = "unexpected_content_encoding"
		discard()
		_ = response.Body.Close()
		return
	}
	payload, e := io.ReadAll(io.LimitReader(response.Body, maxBody+1))
	r.bytesRead = int64(len(payload))
	if e != nil || len(payload) > maxBody {
		discard()
		_ = response.Body.Close()
		if e != nil {
			fail("read_body", e)
		} else {
			r.category = "protocol_error"
			r.stage = "body_limit"
		}
		return
	}
	_ = response.Body.Close()
	r.responseComplete = true
	if response.Close || response.StatusCode < 200 {
		discard()
	}
	switch response.StatusCode {
	case 200:
		if e := validate(w.c, j, payload); e != nil {
			r.category = "body_error"
			r.stage = "validate"
			r.detail = e.Error()
		} else {
			r.category = "correct"
		}
	case 503:
		r.category = "rejected_503"
	case 429:
		r.category = "rejected_429"
	default:
		r.category = "other_http"
		r.detail = truncate(string(payload))
	}
	return
}
func kindAt(workload string, index uint64) string {
	if workload != "mixed" {
		return workload
	}
	switch i := index % 100; {
	case i < 70:
		return "json_object"
	case i < 90:
		return "db"
	case i < 99:
		return "timer"
	default:
		return "cpu"
	}
}

// Merge two independent absolute arrival plans without allocating a plan slice.
// Short wins an exact timestamp tie; CPU inputs/cancellation use its own ordinal.
type taskMixPlan struct {
	origin                           time.Time
	shortRate, cpuRate               float64
	shortTotal, cpuTotal, short, cpu uint64
	index, prefix, cancelEvery       uint64
}

func newTaskMixPlan(c config, origin time.Time) taskMixPlan {
	return taskMixPlan{origin: origin, shortRate: c.rate, cpuRate: c.cpuRate,
		shortTotal: uint64(math.Ceil(c.duration.Seconds() * c.rate)),
		cpuTotal:   uint64(math.Ceil(c.duration.Seconds() * c.cpuRate)),
		prefix:     c.prefix, cancelEvery: c.cpuCancelEvery}
}

func (p *taskMixPlan) next() (job, bool) {
	if p.short == p.shortTotal && p.cpu == p.cpuTotal {
		return job{}, false
	}
	var shortOffset, cpuOffset time.Duration
	if p.short < p.shortTotal {
		shortOffset = time.Duration(float64(p.short) / p.shortRate * float64(time.Second))
	}
	if p.cpu < p.cpuTotal {
		cpuOffset = time.Duration(float64(p.cpu) / p.cpuRate * float64(time.Second))
	}
	short := p.short < p.shortTotal && (p.cpu == p.cpuTotal || shortOffset <= cpuOffset)
	j := job{index: p.index, rid: fmt.Sprintf("%09d%07d", p.prefix, p.index)}
	if short {
		j.kind, j.streamIndex, j.scheduled = "ok", p.short, p.origin.Add(shortOffset)
		p.short++
	} else {
		j.kind, j.streamIndex, j.scheduled = "cpu", p.cpu, p.origin.Add(cpuOffset)
		j.cpuCancel = p.cancelEvery > 0 && (p.cpu+1)%p.cancelEvery == 0
		p.cpu++
	}
	p.index++
	return j, true
}

func validateTaskMix(c config) error {
	if c.duration < time.Millisecond || c.duration > time.Hour || math.IsNaN(c.rate) || math.IsInf(c.rate, 0) || c.rate <= 0 || c.timeout < time.Millisecond || c.timeout > time.Minute || c.concurrency < 1 || c.concurrency > 16384 || c.queue < 0 || c.queue > 65536 || c.prefix == 0 || c.prefix > 999999999 {
		return fmt.Errorf("invalid bounded task_mix configuration")
	}
	if math.IsNaN(c.cpuRate) || math.IsInf(c.cpuRate, 0) || c.cpuRate <= 0 || c.cpuRate > 1000000 || c.rate+c.cpuRate > 1000000 {
		return fmt.Errorf("task_mix requires positive bounded independent --cpu-rate")
	}
	shortPlans, cpuPlans := math.Ceil(c.duration.Seconds()*c.rate), math.Ceil(c.duration.Seconds()*c.cpuRate)
	if shortPlans < 1 || cpuPlans < 1 || shortPlans+cpuPlans > 10000000 {
		return fmt.Errorf("combined task_mix plans exceed10000000")
	}
	cpu, ok := c.protocol.Endpoints["cpu"]
	short, hasShort := c.protocol.Endpoints["ok"]
	if !ok || !hasShort || cpu.Method != "GET" || short.Method != "GET" || len(cpu.Work) != 1 || cpu.Work[0] != 512 || len(cpu.Seeds) != 2 || cpu.Seeds[0] != 3 || cpu.Seeds[1] != 11 {
		return fmt.Errorf("task_mix requires GET ok/cpu and frozen CPU work_units[512],seed_lengths[3,11]")
	}
	if math.IsNaN(c.taskMixShortSLOMS) || math.IsInf(c.taskMixShortSLOMS, 0) || c.taskMixShortSLOMS <= 0 || c.taskMixShortSLOMS > 1000 {
		return fmt.Errorf("invalid task_mix_short_p99_ms")
	}
	if c.cpuCancelEvery != 0 && c.cpuCancelEvery != 10 {
		return fmt.Errorf("task_mix cancellation is disabled or every10CPU plans")
	}
	if c.cpuCancelEvery != 0 && (c.cpuCancelDelay < time.Microsecond || c.cpuCancelDelay > time.Second || c.cpuCancelDelay >= c.timeout) {
		return fmt.Errorf("invalid CPU cancel delay or original request deadline")
	}
	return nil
}
func inputAt(c config, j job) (int, int) {
	e := c.protocol.Endpoints["cpu"]
	if c.workload == "task_mix" {
		return e.Work[j.streamIndex%uint64(len(e.Work))], e.Seeds[(j.streamIndex/uint64(len(e.Work)))%uint64(len(e.Seeds))]
	}
	return e.Work[j.index%uint64(len(e.Work))], e.Seeds[(j.index/uint64(len(e.Work)))%uint64(len(e.Seeds))]
}
func requestSpec(c config, j job) (string, string, []byte) {
	e := c.protocol.Endpoints[j.kind]
	query := url.Values{"rid": {j.rid}}
	var body []byte
	switch {
	case strings.HasPrefix(j.kind, "json_"):
		name := strings.TrimPrefix(j.kind, "json_")
		body = c.corpora[name].bytes
	case j.kind == "cpu":
		units, seed := inputAt(c, j)
		query.Set("work", strings.Repeat("x", units))
		query.Set("seed", strings.Repeat("s", seed))
	case j.kind == "db":
		query.Set("key", strconv.FormatUint(j.index%uint64(c.protocol.Database.Rows)+1, 10))
	}
	return e.Method, e.Path + "?" + query.Encode(), body
}
func decode(p []byte) (any, error) {
	d := json.NewDecoder(bytes.NewReader(p))
	d.UseNumber()
	var v any
	if e := d.Decode(&v); e != nil {
		return nil, e
	}
	var extra any
	if e := d.Decode(&extra); e != io.EOF {
		return nil, fmt.Errorf("trailing JSON data")
	}
	return v, nil
}
func validate(c config, j job, body []byte) error {
	expected := ""
	switch j.kind {
	case "ok":
		expected = "ok:" + j.rid
	case "timer":
		expected = "timer:" + j.rid
	case "wait":
		expected = "wait:" + j.rid
	case "tcp":
		expected = "tcp:" + j.rid
	case "cpu":
		units, seed := inputAt(c, j)
		expected = j.rid + ":" + c.checksums[fmt.Sprintf("%d/%d", units, seed)]
	}
	if expected != "" {
		if string(body) != expected {
			return fmt.Errorf("wrong complete body: %s", truncate(string(body)))
		}
		return nil
	}
	value, e := decode(body)
	if e != nil {
		return e
	}
	object, ok := value.(map[string]any)
	if !ok {
		return fmt.Errorf("response is not JSON object")
	}
	if j.kind == "db" {
		key := int64(j.index%uint64(c.protocol.Database.Rows) + 1)
		if len(object) != 4 || object["id"] != json.Number(strconv.FormatInt(key, 10)) || object["score"] != json.Number(strconv.FormatInt(key*17, 10)) || object["label"] != fmt.Sprintf("item-%d-中文🙂", key) || object["rid"] != j.rid {
			return fmt.Errorf("database JSON fields differ")
		}
		return nil
	}
	corpus := c.corpora[strings.TrimPrefix(j.kind, "json_")]
	if len(object) != 3 || object["id"] != j.rid || object["counter"] != json.Number(strconv.FormatInt(corpus.counter+1, 10)) || !reflect.DeepEqual(object["data"], corpus.data) {
		return fmt.Errorf("JSON id/counter/deep data differ")
	}
	return nil
}

func load(c *config) error {
	raw, e := os.ReadFile(c.protocolPath)
	if e != nil {
		return e
	}
	if len(raw) > 1048576 {
		return fmt.Errorf("protocol exceeds bound")
	}
	sum := sha256.Sum256(raw)
	c.protocolHash = hex.EncodeToString(sum[:])
	if e = json.Unmarshal(raw, &c.protocol); e != nil {
		return e
	}
	if c.protocol.Schema != 1 {
		return fmt.Errorf("unsupported protocol schema")
	}
	if c.protocol.Database.Rows < 1 || c.protocol.Database.Rows > 1000000 {
		return fmt.Errorf("invalid database row bound")
	}
	if len(c.protocol.Endpoints["cpu"].Work) == 0 || len(c.protocol.Endpoints["cpu"].Seeds) == 0 {
		return fmt.Errorf("missing CPU protocol")
	}
	c.checksums = map[string]string{}
	for _, units := range c.protocol.Endpoints["cpu"].Work {
		for _, seed := range c.protocol.Endpoints["cpu"].Seeds {
			if units < 1 || units > 1000 || seed < 1 || seed > 1024 {
				return fmt.Errorf("CPU inputs outside common bounds")
			}
			total := int64(seed)
			for i := 0; i < units*1000; i++ {
				total = (total*33 + int64(i) + int64(seed)) % 100000000
			}
			c.checksums[fmt.Sprintf("%d/%d", units, seed)] = fmt.Sprintf("%08d", total)
		}
	}
	c.corpora = map[string]corpus{}
	for _, name := range []string{"object", "array", "escape"} {
		spec, ok := c.protocol.Corpora[name]
		if !ok {
			return fmt.Errorf("missing corpus %s", name)
		}
		path := filepath.Join(filepath.Dir(c.protocolPath), spec.Path)
		if c.corporaDir != "" {
			path = filepath.Join(c.corporaDir, name+".json")
		}
		if c.corpusPath != "" && c.workload == "json_"+name {
			path = c.corpusPath
		}
		body, e := os.ReadFile(path)
		if e != nil {
			return e
		}
		if len(body) > maxBody {
			return fmt.Errorf("corpus exceeds body bound")
		}
		hash := sha256.Sum256(body)
		digest := hex.EncodeToString(hash[:])
		if digest != spec.SHA256 {
			return fmt.Errorf("corpus hash mismatch %s", name)
		}
		value, e := decode(body)
		if e != nil {
			return e
		}
		v, ok := value.(map[string]any)
		if !ok || len(v) != 2 || v["counter"] != json.Number(strconv.FormatInt(spec.Counter, 10)) {
			return fmt.Errorf("invalid corpus envelope %s", name)
		}
		c.corpora[name] = corpus{bytes: body, data: v["data"], counter: spec.Counter, hash: digest}
	}
	for name, ep := range c.protocol.Endpoints {
		if name == "mixed" {
			continue
		}
		if ep.Method != "GET" && ep.Method != "POST" {
			return fmt.Errorf("invalid endpoint method")
		}
		if !strings.HasPrefix(ep.Path, "/") || strings.ContainsAny(ep.Path, "\r\n?") {
			return fmt.Errorf("invalid endpoint path")
		}
	}
	if c.workload == "task_mix" {
		c.taskMixShortSLOMS = 20
		if raw, ok := c.protocol.SLO["task_mix_short_p99_ms"]; ok {
			if e := json.Unmarshal(raw, &c.taskMixShortSLOMS); e != nil {
				return e
			}
		}
		return validateTaskMix(*c)
	}
	return nil
}
func run(c config) (map[string]any, error) {
	if c.workload == "task_mix" {
		if e := validateTaskMix(c); e != nil {
			return nil, e
		}
	}
	descriptors, err := prepareDescriptorTable(c.concurrency)
	if err != nil {
		return nil, fmt.Errorf("descriptor preparation: %w", err)
	}
	l := &ledger{all: newStats(), types: map[string]*stats{}, seconds: make([]secondStats, int(math.Ceil(c.duration.Seconds())))}
	if c.events != "" {
		w, e := openEvents(c.events, c.workload == "task_mix")
		if e != nil {
			return nil, e
		}
		l.events = w
	}
	queue := make(chan job, c.queue)
	pool := newConnectionPool(c, l)
	var workers sync.WaitGroup
	preparation := prepareRequestWorkers(c, l, pool, queue, &workers, nil)
	preparationResources := l.resourceSnapshot()
	preparationDials, preparationConnections := preparationResources.dials, preparationResources.live
	var anchor monotonicAnchor
	l.origin, anchor = captureMonotonicAnchor(time.Now, readKernelMonotonic)
	l.cutoff = l.origin.Add(c.duration)
	planned := uint64(math.Ceil(c.duration.Seconds() * c.rate))
	if c.workload == "task_mix" {
		planned += uint64(math.Ceil(c.duration.Seconds() * c.cpuRate))
	}
	timer := time.NewTimer(time.Hour)
	if !timer.Stop() {
		<-timer.C
	}
	if c.workload == "task_mix" {
		plan := newTaskMixPlan(c, l.origin)
		for j, ok := plan.next(); ok; j, ok = plan.next() {
			l.plan(j)
			if delay := time.Until(j.scheduled); delay > 0 {
				timer.Reset(delay)
				<-timer.C
			}
			enqueuePlanned(c, l, j, queue, time.Now())
		}
	} else {
		for index := uint64(0); index < planned; index++ {
			j := job{index: index, kind: kindAt(c.workload, index), rid: fmt.Sprintf("%09d%07d", c.prefix, index), scheduled: l.origin.Add(time.Duration(float64(index) / c.rate * float64(time.Second)))}
			l.plan(j)
			if delay := time.Until(j.scheduled); delay > 0 {
				timer.Reset(delay)
				<-timer.C
			}
			enqueuePlanned(c, l, j, queue, time.Now())
		}
	}
	timer.Stop()
	if delay := time.Until(l.cutoff); delay > 0 {
		time.Sleep(delay)
	}
	close(queue)
	workers.Wait()
	pool.close()
	finished := time.Now()
	if l.events != nil {
		l.events.close()
	}
	finalResources := l.resourceSnapshot()
	valid := !l.budget.overflow && finalResources.active == 0 && finalResources.live == 0
	notStarted := uint64(0)
	for k, n := range l.all.Results {
		if strings.HasPrefix(k, "not_started_") {
			notStarted += n
		}
	}
	valid = valid && notStarted == 0
	byType := map[string]any{}
	for kind, s := range l.types {
		byType[kind] = s.view(c.duration.Seconds())
		if c.workload == "task_mix" {
			byType[kind] = s.taskMixView(c.duration.Seconds(), sloMS(kind, c))
		}
	}
	seconds := make([]map[string]any, len(l.seconds))
	for i, s := range l.seconds {
		seconds[i] = map[string]any{"second": i, "planned": s.planned, "attempted": s.attempted, "sent": s.sent, "correct_completed_in_window_from_this_plan_second": s.correct, "results": s.results, "by_type": s.kinds, "correct_scheduled_to_complete": s.latency.value(), "correct_by_completion_second": s.completedCorrect, "correct_by_completion_second_and_type": s.completedKinds}
	}
	events := map[string]any{"enabled": false}
	if l.events != nil {
		events = map[string]any{"enabled": true, "path": l.events.path, "rows": l.events.written, "dropped": l.events.dropped, "error": l.events.err, "queue_limit": 8192, "uncompressed_bytes_limit": maxEvents}
		valid = valid && l.events.dropped == 0 && l.events.err == "" && l.events.written == planned
	}
	hashes := map[string]string{}
	for name, data := range c.corpora {
		hashes[name] = data.hash
	}
	out := map[string]any{"schema": "ku-cross-stack-client-v5", "valid_driver": valid, "ledger": l.all.view(c.duration.Seconds()), "by_type": byType, "per_second": seconds, "first_errors": l.samples, "events": events, "resources": map[string]any{"pid": os.Getpid(), "go_version": runtime.Version(), "gomaxprocs": runtime.GOMAXPROCS(0), "dials": finalResources.dials, "pool_idle_expirations": finalResources.poolExpired, "requests_per_dial": float64(l.all.Sent) / float64(max(finalResources.dials, 1)), "peak_connections": finalResources.peakLive, "connections_after_cleanup": finalResources.live, "peak_inflight": finalResources.peakActive, "active_after_cleanup": finalResources.active, "queue_peak": finalResources.peakQueue, "histogram_unique_buckets": l.budget.keys, "histogram_cells_per_distribution": maxHistogramKeys, "histogram_overflow": l.budget.overflow}, "configuration": map[string]any{"address": c.address, "workload": c.workload, "protocol_sha256": c.protocolHash, "corpora_sha256": hashes, "duration_seconds": c.duration.Seconds(), "rate": c.rate, "concurrency": c.concurrency, "queue": c.queue, "timeout_seconds": c.timeout.Seconds(), "absolute_deadline_origin": "scheduled_at; includes client queue, dial, write and complete response", "keepalive_idle_ms": c.idle.Milliseconds(), "rid_prefix": c.prefix, "no_retry": true, "http": "1.1 keep-alive; no TLS/proxy/pipeline/compression", "primary_latency": "scheduled_to_complete", "sent_definition": "first socket Write that returned n>0, timestamp before that Write; dial failures are attempted but unsent", "event_clock": "nanoseconds relative to run origin, Go monotonic clock; sent null if no bytes written", "origin_unix_ns": l.origin.UnixNano(), "cleanup_seconds": finished.Sub(l.cutoff).Seconds(), "histogram_minimum_resolution_us": 10, "histogram_maximum_relative_bucket_width": 1.0 / 128, "event_compression": "gzip.BestSpeed", "validation_in_driver_cost": true}}

	clocks := out["configuration"].(map[string]any)
	out["descriptor_preparation"] = descriptors.report(l.origin, anchor)
	out["worker_preparation"] = map[string]any{
		"ready_workers":          preparation.workers,
		"configured_workers":     c.concurrency,
		"started_unix_ns":        preparation.started.UnixNano(),
		"ready_unix_ns":          preparation.ready.UnixNano(),
		"duration_ns":            preparation.ready.Sub(preparation.started).Nanoseconds(),
		"started_offset_ns":      preparation.started.Sub(l.origin).Nanoseconds(),
		"ready_offset_ns":        preparation.ready.Sub(l.origin).Nanoseconds(),
		"started_monotonic_ns":   anchor.at(preparation.started.Sub(l.origin).Nanoseconds()),
		"ready_monotonic_ns":     anchor.at(preparation.ready.Sub(l.origin).Nanoseconds()),
		"origin_after_ready_ns":  l.origin.Sub(preparation.ready).Nanoseconds(),
		"complete_before_origin": !preparation.ready.After(l.origin),
		"connections_dialed":     preparationDials,
		"connections_live":       preparationConnections,
		"readiness_boundary":     "all workers constructed immediately before reading the empty request queue; no origin broadcast; not a claim that every goroutine is already parked",
		"preparation_accounting": "real driver-process startup outside the arrival window; no service reset, cgroup refund, connection prewarm or subtraction of startup CPU; report lifetime/startup costs separately",
	}
	clocks["planned_cutoff_offset_ns"] = c.duration.Nanoseconds()
	clocks["planned_cutoff_unix_ns"] = l.cutoff.UnixNano()
	clocks["last_actual_sent_offset_ns"] = offset(l.lastActualSent, l.origin)
	clocks["last_actual_sent_unix_ns"] = unixNano(l.lastActualSent)
	clocks["last_actual_sent_rid"] = l.lastActualSentRID
	clocks["last_actual_completed_offset_ns"] = offset(l.lastActualCompleted, l.origin)
	clocks["last_actual_completed_unix_ns"] = unixNano(l.lastActualCompleted)
	clocks["last_actual_completed_rid"] = l.lastActualCompletedRID
	clocks["last_complete_response_offset_ns"] = offset(l.lastCompleteResponse, l.origin)
	clocks["last_complete_response_unix_ns"] = unixNano(l.lastCompleteResponse)
	clocks["last_terminal_offset_ns"] = offset(l.lastTerminal, l.origin)
	clocks["last_terminal_unix_ns"] = unixNano(l.lastTerminal)
	clocks["last_terminal_monotonic_ns"] = anchor.at(offset(l.lastTerminal, l.origin))
	clocks["last_terminal_rid"] = l.lastTerminalRID
	clocks["last_terminal_definition"] = "last terminal among all plans, including unsent/not-started plans and planned resets; not server physical completion"
	anchor.annotate(clocks)
	clocks["last_actual_completed_definition"] = "last terminal timestamp among actually sent plans, including timeout/transport errors; full response tracked separately"
	clocks["tail_policy"] = "planned arrivals may start after cutoff only before their original scheduled_at+timeout; late window completions never increase window goodput"
	clocks["actual_clock_frozen"] = "after all planned requests reached one terminal state; does not renew the planned cutoff or request deadlines"
	if c.workload == "task_mix" {
		clocks["generated_definition"] = "arrival dispatcher timestamp before bounded client queue; existing clock sample, includes generator scheduling/plan work but excludes worker queue/dial/Write"
		clocks["cpu_rate"] = c.cpuRate
		clocks["task_mix_short_SLO_ms"] = c.taskMixShortSLOMS
		clocks["cpu_cancel_every"] = c.cpuCancelEvery
		clocks["cpu_cancel_delay_us"] = c.cpuCancelDelay.Microseconds()
		out["task_mix"] = map[string]any{"short_type": "ok", "long_type": "cpu", "short_rate": c.rate, "cpu_rate": c.cpuRate, "short_SLO_ms": c.taskMixShortSLOMS, "arrival_plan": "independent absolute streams, same-time short first; CPU sequence selects seeds[3,11]; optional reset every10th CPU plan", "normal_long_denominator": "CPU planned minus predesignated cancellation plans; all cancelled/unsent/error rows remain in full ledger and planned_cancellation", "reset_samples": l.cancelSamples, "reset_scope": "full request write then sent+delay TCP SetLinger(0)+Close; timestamp precedes successful Close, not proof peer receipt, execution stop or resource reuse", "cancel_seed_scope": "with alternating[3,11] and every10th plan, predesignated resets use seed11", "CSV_extra_fields": []string{"stream_index", "planned_cpu_cancel", "reset_due_ns", "reset_issued_ns", "request_write_completed", "generated_ns"}}
	}
	runEnd := time.Now()
	clocks["run_end_offset_ns"] = offset(runEnd, l.origin)
	clocks["run_end_unix_ns"] = unixNano(runEnd)
	clocks["run_end_monotonic_ns"] = anchor.at(offset(runEnd, l.origin))
	clocks["run_end_definition"] = "after terminal accounting, connection retirement and CSV flush; before final JSON serialization/process exit"
	return out, nil
}
func parse() (config, error) {
	c := config{}
	var duration, timeout float64
	var idleMS int
	var cancelDelayUS int64
	flag.StringVar(&c.address, "address", "", "HTTP host:port")
	flag.StringVar(&c.workload, "workload", "ok", "ok/json_object/json_array/json_escape/cpu/timer/wait/tcp/db/mixed/task_mix")
	flag.StringVar(&c.protocolPath, "protocol", "tests/benchmarks/cross_stack/protocol.json", "frozen protocol JSON")
	flag.StringVar(&c.corporaDir, "corpora-dir", "", "optional corpus directory, frozen hashes still verified")
	flag.StringVar(&c.corpusPath, "corpus", "", "override selected JSON corpus path, frozen hash still verified")
	flag.StringVar(&c.output, "output", "", "exclusive-create JSON output, stdout if omitted")
	flag.StringVar(&c.events, "events", "", "exclusive-create streaming gzip CSV, queue 8192; overflow invalidates run")
	flag.Float64Var(&duration, "duration", 30, "arrival window seconds (0..3600)")
	flag.Float64Var(&c.rate, "rate", 100, "open arrivals per second (positive, total <=10000000)")
	flag.Float64Var(&c.cpuRate, "cpu-rate", 0, "task_mix independent CPU arrivals/sec; --rate is short arrivals/sec")
	flag.Uint64Var(&c.cpuCancelEvery, "cpu-cancel-every", 0, "task_mix:0 disabled or10 for every10th CPU plan, normal denominator excludes these plans")
	flag.Int64Var(&cancelDelayUS, "cpu-cancel-delay-us", 250, "planned CPU RST due microseconds after first positive request write")
	flag.IntVar(&c.concurrency, "concurrency", 128, "maximum in-flight requests and idle pool capacity (1..16384)")
	flag.IntVar(&c.queue, "queue", 256, "bounded pending plan descriptors (0..65536)")
	flag.Float64Var(&timeout, "timeout", 3, "absolute seconds from scheduled_at, including queue/dial/write/read")
	flag.IntVar(&idleMS, "keepalive-idle-ms", 500, "retire expired shared idle connections before the next distinct plan")
	flag.Uint64Var(&c.prefix, "rid-prefix", 0, "numeric run prefix 1..999999999; zero generates random prefix")
	flag.Parse()
	if _, _, e := net.SplitHostPort(c.address); e != nil {
		return c, e
	}
	if math.IsNaN(duration) || math.IsInf(duration, 0) || duration < .001 || duration > 3600 || math.IsNaN(c.rate) || math.IsInf(c.rate, 0) || c.rate <= 0 || c.rate > 1000000 || duration*c.rate > 10000000 || math.IsNaN(timeout) || math.IsInf(timeout, 0) || timeout < .001 || timeout > 60 || c.concurrency < 1 || c.concurrency > 16384 || c.queue < 0 || c.queue > 65536 || idleMS < 0 || idleMS > 60000 || c.prefix > 999999999 {
		return c, fmt.Errorf("invalid bounded configuration")
	}
	c.duration = time.Duration(duration * float64(time.Second))
	c.timeout = time.Duration(timeout * float64(time.Second))
	c.idle = time.Duration(idleMS) * time.Millisecond
	if math.IsNaN(c.cpuRate) || math.IsInf(c.cpuRate, 0) || c.cpuRate < 0 || c.cpuRate > 1000000 || cancelDelayUS < 1 || cancelDelayUS > 1000000 {
		return c, fmt.Errorf("invalid bounded CPU arrival/cancel configuration")
	}
	if c.workload != "task_mix" && (c.cpuRate != 0 || c.cpuCancelEvery != 0) {
		return c, fmt.Errorf("independent CPU arrival/cancellation requires task_mix")
	}
	c.cpuCancelDelay = time.Duration(cancelDelayUS) * time.Microsecond
	if c.prefix == 0 {
		var raw [8]byte
		if _, e := rand.Read(raw[:]); e != nil {
			return c, e
		}
		c.prefix = binary.LittleEndian.Uint64(raw[:])%999999999 + 1
	}
	if e := load(&c); e != nil {
		return c, e
	}
	if _, ok := c.protocol.Endpoints[c.workload]; !ok && c.workload != "task_mix" {
		return c, fmt.Errorf("unsupported workload")
	}
	return c, nil
}
func main() {
	c, e := parse()
	if e == nil {
		var out map[string]any
		out, e = run(c)
		if e == nil {
			var data []byte
			data, e = json.Marshal(out)
			if len(data) > 16*1048576 {
				e = fmt.Errorf("JSON report exceeds 16MiB bound")
			}
			if e == nil {
				data = append(data, '\n')
				if c.output == "" {
					_, e = os.Stdout.Write(data)
				} else {
					var f *os.File
					f, e = os.OpenFile(c.output, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
					if e == nil {
						_, e = f.Write(data)
						if closeErr := f.Close(); e == nil {
							e = closeErr
						}
					}
				}
			}
		}
	}
	if e != nil {
		fmt.Fprintln(os.Stderr, e)
		os.Exit(2)
	}
}
