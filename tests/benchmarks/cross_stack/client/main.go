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
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

const maxHeader = 32768
const maxBody = 65536
const maxHistogramKeys = 1000000 // Shared hard bound, not per histogram.
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
	concurrency, queue                                                      int
	prefix                                                                  uint64
	protocol                                                                protocol
	protocolHash                                                            string
	corpora                                                                 map[string]corpus
	checksums                                                               map[string]string
}
type job struct {
	index     uint64
	kind, rid string
	scheduled time.Time
}
type result struct {
	category, stage, detail               string
	status                                int
	sent, completed                       time.Time
	bytesWritten, bytesRead               int64
	writeComplete, responseComplete, late bool
}
type histogram struct {
	bins  map[int64]uint64
	count uint64
	maxNS int64
	sum   float64
}
type histogramBudget struct {
	keys     int
	overflow bool
}

func (h *histogram) add(d time.Duration, b *histogramBudget) {
	n := max(int64(d), 0)
	bucket := n / histogramNS
	if h.bins == nil {
		h.bins = make(map[int64]uint64)
	}
	if _, ok := h.bins[bucket]; !ok {
		if b.keys >= maxHistogramKeys {
			b.overflow = true
			return
		}
		b.keys++
	}
	h.bins[bucket]++
	h.count++
	h.sum += float64(n)
	h.maxNS = max(h.maxNS, n)
}
func (h *histogram) value() map[string]any {
	v := map[string]any{"count": h.count, "resolution_us": 10, "max_ms": float64(h.maxNS) / 1e6, "quantiles": "upper bucket edge; not averaged across rounds"}
	if h.count == 0 {
		v["p50_ms"] = nil
		v["p95_ms"] = nil
		v["p99_ms"] = nil
		v["p999_ms"] = nil
		return v
	}
	keys := make([]int64, 0, len(h.bins))
	for k := range h.bins {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool { return keys[i] < keys[j] })
	for _, q := range []struct {
		name     string
		fraction float64
		minimum  uint64
	}{{"p50_ms", .5, 1}, {"p95_ms", .95, 20}, {"p99_ms", .99, 100}, {"p999_ms", .999, 1000}} {
		if h.count < q.minimum {
			v[q.name] = nil
			continue
		}
		target := uint64(math.Ceil(float64(h.count) * q.fraction))
		var total uint64
		for _, k := range keys {
			total += h.bins[k]
			if total >= target {
				v[q.name] = float64((k+1)*histogramNS) / 1e6
				break
			}
		}
	}
	v["mean_ms"] = h.sum / float64(h.count) / 1e6
	return v
}

type stats struct {
	Planned                                          uint64            `json:"planned"`
	Attempted                                        uint64            `json:"attempted"`
	Sent                                             uint64            `json:"sent"`
	Results                                          map[string]uint64 `json:"results"`
	CorrectWindow                                    uint64            `json:"correct_in_window"`
	Late                                             uint64            `json:"late_response_events"`
	SLOFailed                                        uint64            `json:"correct_over_slo"`
	Written                                          int64             `json:"request_bytes_written"`
	Read                                             int64             `json:"response_bytes_read"`
	correctScheduled, correctSent, allScheduled, lag histogram
	sentErrors                                       uint64
}

func newStats() *stats { return &stats{Results: map[string]uint64{}} }
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
	return map[string]any{"planned": s.Planned, "attempted": s.Attempted, "sent": s.Sent, "terminal": terminal, "results": s.Results, "planned_reconciled": terminal == s.Planned, "correct_in_window": s.CorrectWindow, "goodput_rps": float64(s.CorrectWindow) / duration, "correct_total": correct, "failure_fraction_planned": failurePlanned, "failure_fraction_sent": failureSent, "correct_over_slo": s.SLOFailed, "late_response_events": s.Late, "request_bytes_written": s.Written, "response_bytes_read": s.Read, "correct_scheduled_to_complete": s.correctScheduled.value(), "correct_sent_to_complete": s.correctSent.value(), "all_scheduled_to_terminal": s.allScheduled.value(), "scheduled_to_sent": s.lag.value()}
}

type secondStats struct {
	planned, attempted, sent, correct uint64
	results                           map[string]uint64
	latency                           histogram
	kinds                             map[string]map[string]uint64
	completedCorrect                  uint64
	completedKinds                    map[string]uint64
}
type ledger struct {
	mu                                                        sync.Mutex
	all                                                       *stats
	types                                                     map[string]*stats
	seconds                                                   []secondStats
	budget                                                    histogramBudget
	origin, cutoff                                            time.Time
	lastActualSent, lastActualCompleted, lastCompleteResponse time.Time
	lastActualSentRID, lastActualCompletedRID                 string
	active, peakActive, live, peakLive, dials, peakQueue      uint64
	samples                                                   []map[string]any
	events                                                    *eventWriter
}

func (l *ledger) typed(kind string) *stats {
	s := l.types[kind]
	if s == nil {
		s = newStats()
		l.types[kind] = s
	}
	return s
}
func (l *ledger) plan(j job) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.all.Planned++
	l.typed(j.kind).Planned++
	i := int(j.scheduled.Sub(l.origin) / time.Second)
	if i >= len(l.seconds) {
		i = len(l.seconds) - 1
	}
	l.seconds[i].planned++
}
func (l *ledger) start() {
	l.mu.Lock()
	l.active++
	l.peakActive = max(l.peakActive, l.active)
	l.mu.Unlock()
}
func sloMS(kind string, c config) float64 {
	key := "json_p99_ms"
	switch kind {
	case "ok":
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
		l.active--
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
		if attempted {
			s.Attempted++
		}
		if sent {
			s.Sent++
			if r.category != "correct" {
				s.sentErrors++
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
			s.correctScheduled.add(r.completed.Sub(j.scheduled), &l.budget)
			if sent {
				s.correctSent.add(r.completed.Sub(r.sent), &l.budget)
			}
			if !r.completed.After(l.cutoff) {
				s.CorrectWindow++
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
	if r.category != "correct" && len(l.samples) < 24 {
		l.samples = append(l.samples, map[string]any{"rid": j.rid, "index": j.index, "type": j.kind, "terminal": r.category, "status": r.status, "stage": r.stage, "detail": truncate(r.detail), "scheduled_ns": j.scheduled.Sub(l.origin).Nanoseconds(), "sent_ns": offset(r.sent, l.origin), "completed_ns": r.completed.Sub(l.origin).Nanoseconds(), "bytes_written": r.bytesWritten, "bytes_read": r.bytesRead})
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

func openEvents(path string) (*eventWriter, error) {
	f, e := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if e != nil {
		return nil, e
	}
	w := &eventWriter{queue: make(chan event, 8192), done: make(chan struct{}), path: path}
	go func() {
		defer close(w.done)
		z := gzip.NewWriter(f)
		csvw := csv.NewWriter(z)
		err := csvw.Write([]string{"rid", "type", "index", "scheduled_ns", "sent_ns", "completed_ns", "attempt_started_ns", "terminal", "status", "request_bytes_written", "response_bytes_read", "late_response"})
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
			record := []string{row.j.rid, row.j.kind, strconv.FormatUint(row.j.index, 10), format(row.j.scheduled), format(row.result.sent), format(row.result.completed), format(row.began), row.result.category, strconv.Itoa(row.result.status), strconv.FormatInt(row.result.bytesWritten, 10), strconv.FormatInt(row.result.bytesRead, 10), strconv.FormatBool(row.result.late)}
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

type worker struct {
	c      config
	l      *ledger
	conn   *countedConn
	reader *bufio.Reader
	last   time.Time
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
	// The cutoff defines the arrival plan and the throughput window, not a
	// replacement deadline. A delayed plan retains only its original budget.
	if !now.Before(j.scheduled.Add(c.timeout)) {
		l.finish(j, time.Time{}, result{category: "not_started_scheduler_deadline", completed: now}, c)
		return
	}
	select {
	case queue <- j:
		l.mu.Lock()
		l.peakQueue = max(l.peakQueue, uint64(len(queue)))
		l.mu.Unlock()
	default:
		l.finish(j, time.Time{}, result{category: "not_started_queue", completed: now}, c)
	}
}

func (w *worker) close() {
	if w.conn != nil {
		_ = w.conn.Close()
		w.conn = nil
		w.reader = nil
		w.l.mu.Lock()
		w.l.live--
		w.l.mu.Unlock()
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
	if w.conn != nil && w.c.idle > 0 && began.Sub(w.last) >= w.c.idle {
		w.close()
	}
	var connection *countedConn
	var before int64
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
		if r.category == "timeout" || r.category == "connection_error" || r.category == "protocol_error" || r.category == "body_error" {
			w.close()
		} else if w.conn != nil {
			w.last = r.completed
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
	if w.conn == nil {
		w.l.mu.Lock()
		w.l.dials++
		w.l.mu.Unlock()
		ctx, cancel := context.WithDeadline(context.Background(), deadline)
		conn, e := (&net.Dialer{FallbackDelay: -1}).DialContext(ctx, "tcp", w.c.address)
		cancel()
		if e != nil {
			fail("dial", e)
			return
		}
		w.conn = &countedConn{Conn: conn}
		w.reader = bufio.NewReaderSize(w.conn, 8192)
		w.l.mu.Lock()
		w.l.live++
		w.l.peakLive = max(w.l.peakLive, w.l.live)
		w.l.mu.Unlock()
	}
	connection = w.conn
	before = connection.written
	connection.first = time.Time{}
	if e := connection.SetDeadline(deadline); e != nil {
		fail("deadline", e)
		return
	}
	method, path, body := requestSpec(w.c, j)
	headers := http.Header{"Connection": {"keep-alive"}, "Accept-Encoding": {"identity"}, "X-Cross-Stack-Request": {j.rid}}
	if body != nil {
		headers.Set("Content-Type", "application/json")
	}
	req := &http.Request{Method: method, URL: &url.URL{Opaque: path}, Host: w.c.address, Proto: "HTTP/1.1", ProtoMajor: 1, ProtoMinor: 1, Header: headers, ContentLength: int64(len(body))}
	if body != nil {
		req.Body = io.NopCloser(bytes.NewReader(body))
	}
	if e := req.Write(connection); e != nil {
		fail("write", e)
		return
	}
	r.writeComplete = true
	response, e := readResponse(w.reader, req)
	if e != nil {
		fail("read_parse_header", e)
		return
	}
	r.status = response.StatusCode
	if encoding := response.Header.Get("Content-Encoding"); encoding != "" && encoding != "identity" {
		r.category = "protocol_error"
		r.stage = "unexpected_content_encoding"
		w.close()
		_ = response.Body.Close()
		return
	}
	payload, e := io.ReadAll(io.LimitReader(response.Body, maxBody+1))
	r.bytesRead = int64(len(payload))
	if e != nil || len(payload) > maxBody {
		w.close()
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
		w.close()
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
func inputAt(c config, j job) (int, int) {
	e := c.protocol.Endpoints["cpu"]
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
	return nil
}
func run(c config) (map[string]any, error) {
	l := &ledger{all: newStats(), types: map[string]*stats{}, seconds: make([]secondStats, int(math.Ceil(c.duration.Seconds())))}
	if c.events != "" {
		w, e := openEvents(c.events)
		if e != nil {
			return nil, e
		}
		l.events = w
	}
	gate := make(chan struct{})
	queue := make(chan job, c.queue)
	var workers sync.WaitGroup
	for i := 0; i < c.concurrency; i++ {
		workers.Add(1)
		go func() {
			defer workers.Done()
			w := worker{c: c, l: l}
			defer w.close()
			<-gate
			for j := range queue {
				w.execute(j)
			}
		}()
	}
	l.origin = time.Now()
	l.cutoff = l.origin.Add(c.duration)
	close(gate)
	planned := uint64(math.Ceil(c.duration.Seconds() * c.rate))
	timer := time.NewTimer(time.Hour)
	if !timer.Stop() {
		<-timer.C
	}
	for index := uint64(0); index < planned; index++ {
		j := job{index: index, kind: kindAt(c.workload, index), rid: fmt.Sprintf("%09d%07d", c.prefix, index), scheduled: l.origin.Add(time.Duration(float64(index) / c.rate * float64(time.Second)))}
		l.plan(j)
		if delay := time.Until(j.scheduled); delay > 0 {
			timer.Reset(delay)
			<-timer.C
		}
		enqueuePlanned(c, l, j, queue, time.Now())
	}
	timer.Stop()
	if delay := time.Until(l.cutoff); delay > 0 {
		time.Sleep(delay)
	}
	close(queue)
	workers.Wait()
	finished := time.Now()
	if l.events != nil {
		l.events.close()
	}
	valid := !l.budget.overflow && l.active == 0 && l.live == 0
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
	out := map[string]any{"schema": "ku-cross-stack-client-v2", "valid_driver": valid, "ledger": l.all.view(c.duration.Seconds()), "by_type": byType, "per_second": seconds, "first_errors": l.samples, "events": events, "resources": map[string]any{"pid": os.Getpid(), "go_version": runtime.Version(), "gomaxprocs": runtime.GOMAXPROCS(0), "dials": l.dials, "peak_connections": l.peakLive, "connections_after_cleanup": l.live, "peak_inflight": l.peakActive, "active_after_cleanup": l.active, "queue_peak": l.peakQueue, "histogram_unique_buckets": l.budget.keys, "histogram_bucket_limit": maxHistogramKeys, "histogram_overflow": l.budget.overflow}, "configuration": map[string]any{"address": c.address, "workload": c.workload, "protocol_sha256": c.protocolHash, "corpora_sha256": hashes, "duration_seconds": c.duration.Seconds(), "rate": c.rate, "concurrency": c.concurrency, "queue": c.queue, "timeout_seconds": c.timeout.Seconds(), "absolute_deadline_origin": "scheduled_at; includes client queue, dial, write and complete response", "keepalive_idle_ms": c.idle.Milliseconds(), "rid_prefix": c.prefix, "no_retry": true, "http": "1.1 keep-alive; no TLS/proxy/pipeline/compression", "primary_latency": "scheduled_to_complete", "sent_definition": "first socket Write that returned n>0, timestamp before that Write; dial failures are attempted but unsent", "event_clock": "nanoseconds relative to run origin, Go monotonic clock; sent null if no bytes written", "origin_unix_ns": l.origin.UnixNano(), "cleanup_seconds": finished.Sub(l.cutoff).Seconds(), "histogram_resolution_us": 10, "validation_in_driver_cost": true}}

	clocks := out["configuration"].(map[string]any)
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
	clocks["last_actual_completed_definition"] = "last terminal timestamp among actually sent plans, including timeout/transport errors; full response tracked separately"
	clocks["tail_policy"] = "planned arrivals may start after cutoff only before their original scheduled_at+timeout; late window completions never increase window goodput"
	clocks["actual_clock_frozen"] = "after all planned requests reached one terminal state; does not renew the planned cutoff or request deadlines"
	return out, nil
}
func parse() (config, error) {
	c := config{}
	var duration, timeout float64
	var idleMS int
	flag.StringVar(&c.address, "address", "", "HTTP host:port")
	flag.StringVar(&c.workload, "workload", "ok", "ok/json_object/json_array/json_escape/cpu/timer/wait/tcp/db/mixed")
	flag.StringVar(&c.protocolPath, "protocol", "tests/benchmarks/cross_stack/protocol.json", "frozen protocol JSON")
	flag.StringVar(&c.corporaDir, "corpora-dir", "", "optional corpus directory, frozen hashes still verified")
	flag.StringVar(&c.corpusPath, "corpus", "", "override selected JSON corpus path, frozen hash still verified")
	flag.StringVar(&c.output, "output", "", "exclusive-create JSON output, stdout if omitted")
	flag.StringVar(&c.events, "events", "", "exclusive-create streaming gzip CSV, queue 8192; overflow invalidates run")
	flag.Float64Var(&duration, "duration", 30, "arrival window seconds (0..3600)")
	flag.Float64Var(&c.rate, "rate", 100, "open arrivals per second (positive, total <=10000000)")
	flag.IntVar(&c.concurrency, "concurrency", 128, "fixed workers/connections (1..16384)")
	flag.IntVar(&c.queue, "queue", 256, "bounded pending plan descriptors (0..65536)")
	flag.Float64Var(&timeout, "timeout", 3, "absolute seconds from scheduled_at, including queue/dial/write/read")
	flag.IntVar(&idleMS, "keepalive-idle-ms", 500, "retire a previous idle connection before the next distinct plan")
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
	if _, ok := c.protocol.Endpoints[c.workload]; !ok {
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
