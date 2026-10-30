package main

import (
	"bufio"
	"compress/gzip"
	"encoding/csv"
	"encoding/json"
	"fmt"
	"io"
	"math"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func testConfig(t *testing.T) config {
	t.Helper()
	c := config{protocolPath: "../protocol.json", workload: "ok", duration: 20 * time.Millisecond, timeout: time.Second, rate: 50, concurrency: 1, queue: 4, prefix: 1}
	if e := load(&c); e != nil {
		t.Fatal(e)
	}
	return c
}
func testLedger() *ledger { return &ledger{all: newStats(), types: map[string]*stats{}} }
func testWorker(c config, l *ledger) worker {
	return worker{c: c, l: l, pool: newConnectionPool(c, l)}
}

func TestGoldenResponseFieldsAndDeepData(t *testing.T) {
	c := testConfig(t)
	for _, kind := range []string{"ok", "json_object", "json_array", "json_escape", "cpu", "timer", "wait", "tcp", "db"} {
		t.Run(kind, func(t *testing.T) {
			j := job{index: 3, kind: kind, rid: "0000000010000003"}
			var value any
			switch kind {
			case "ok":
				value = "ok:" + j.rid
			case "timer":
				value = "timer:" + j.rid
			case "wait":
				value = "wait:" + j.rid
			case "tcp":
				value = "tcp:" + j.rid
			case "cpu":
				units, seed := inputAt(c, j)
				value = j.rid + ":" + c.checksums[fmt.Sprintf("%d/%d", units, seed)]
			case "db":
				value = map[string]any{"id": 4, "score": 68, "label": "item-4-中文🙂", "rid": j.rid}
			default:
				corpus := c.corpora[strings.TrimPrefix(kind, "json_")]
				value = map[string]any{"id": j.rid, "counter": 8, "data": corpus.data}
			}
			var body []byte
			if text, ok := value.(string); ok {
				body = []byte(text)
			} else {
				body, _ = json.Marshal(value)
			}
			if e := validate(c, j, body); e != nil {
				t.Fatal(e)
			}
			if e := validate(c, j, append(body, []byte("garbage")...)); e == nil {
				t.Fatal("accepted incorrect complete response")
			}
			if strings.HasPrefix(kind, "json_") {
				wrong := map[string]any{"id": j.rid, "counter": 8, "data": nil}
				bad, _ := json.Marshal(wrong)
				if e := validate(c, j, bad); e == nil {
					t.Fatal("accepted wrong nested data")
				}
			}
		})
	}
	// Golden integer arithmetic independently fixes the selected protocol result.
	j := job{kind: "cpu", index: 5, rid: "7"}
	units, seed := inputAt(c, j)
	if units != 512 || seed != 3 || c.checksums["512/3"] != "22944003" {
		t.Fatalf("unexpected selected golden CPU input/result: %d %d %s", units, seed, c.checksums["512/3"])
	}
	counts := map[string]int{}
	for i := uint64(0); i < 100; i++ {
		counts[kindAt("mixed", i)]++
	}
	if counts["json_object"] != 70 || counts["db"] != 20 || counts["timer"] != 9 || counts["cpu"] != 1 {
		t.Fatal(counts)
	}
}

func TestRawTCPFailureNeverReplays(t *testing.T) {
	c := testConfig(t)
	listener, e := net.ListenTCP("tcp", &net.TCPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if e != nil {
		t.Fatal(e)
	}
	defer listener.Close()
	c.address = listener.Addr().String()
	var accepted atomic.Uint32
	done := make(chan struct{})
	go func() {
		defer close(done)
		_ = listener.SetDeadline(time.Now().Add(300 * time.Millisecond))
		for {
			conn, e := listener.Accept()
			if e != nil {
				return
			}
			accepted.Add(1)
			req, e := http.ReadRequest(bufio.NewReader(conn))
			if e == nil {
				_, _ = io.Copy(io.Discard, req.Body)
				_ = req.Body.Close()
			}
			_ = conn.Close()
		}
	}()
	w := testWorker(c, testLedger())
	j := job{kind: "ok", rid: "1", scheduled: time.Now()}
	result := w.request(j, j.scheduled)
	w.pool.close()
	if result.category != "connection_error" || result.sent.IsZero() {
		t.Fatalf("wrong failure classification: %+v", result)
	}
	<-done
	if accepted.Load() != 1 {
		t.Fatalf("failed request replayed %d times", accepted.Load())
	}
}

func TestSharedPoolReusesAcrossWorkersAtLowArrivalRate(t *testing.T) {
	c := testConfig(t)
	c.concurrency, c.queue, c.idle = 256, 65536, 500*time.Millisecond
	addresses := make(chan string, 4)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("X-Request-ID") != r.URL.Query().Get("rid") {
			t.Error("missing correlated request ID")
		}
		addresses <- r.RemoteAddr
		_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	l := testLedger()
	pool := newConnectionPool(c, l)
	defer pool.close()
	workers := []worker{{c: c, l: l, pool: pool}, {c: c, l: l, pool: pool}}
	for i := range workers {
		j := job{kind: "ok", rid: fmt.Sprint(i + 1), scheduled: time.Now()}
		r := workers[i].request(j, j.scheduled)
		if r.category != "correct" || r.bytesWritten == 0 || r.sent.Before(j.scheduled) {
			t.Fatalf("worker %d lost its own request clocks/bytes: %+v", i, r)
		}
		if i == 0 {
			time.Sleep(20 * time.Millisecond)
		}
	}
	if first, second := <-addresses, <-addresses; first != second || l.dials != 1 || l.live != 1 {
		t.Fatalf("different workers failed to reuse socket: %s %s, dials=%d live=%d", first, second, l.dials, l.live)
	}
	// An expired next plan neither checks out the idle socket nor writes bytes.
	j := job{kind: "ok", rid: "expired", scheduled: time.Now().Add(-2 * c.timeout)}
	if r := workers[1].request(j, time.Now()); r.category != "not_started_deadline" || !r.sent.IsZero() || r.bytesWritten != 0 || l.dials != 1 || len(pool.idle) != 1 {
		t.Fatalf("expired plan touched a pooled socket: %+v", r)
	}
	pool.close()
	if l.live != 0 || len(pool.idle) != 0 {
		t.Fatalf("shared pool leaked connections: live=%d idle=%d", l.live, len(pool.idle))
	}
	// Exercise the actual 256-worker run path at only 20 arrivals/second.
	c.duration, c.rate = 100*time.Millisecond, 20
	report, err := run(c)
	if err != nil {
		t.Fatal(err)
	}
	resources := report["resources"].(map[string]any)
	if report["schema"] != "ku-cross-stack-client-v5" || report["valid_driver"] != true || resources["dials"] != uint64(1) || resources["connections_after_cleanup"] != uint64(0) || resources["active_after_cleanup"] != uint64(0) {
		t.Fatalf("run path did not share/close idle connections: %#v", report)
	}
}

func TestHistoricalLedgerDoesNotBlockRequestResourceAccounting(t *testing.T) {
	c := testConfig(t)
	var requests atomic.Uint32
	seen := make(chan string, 1)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests.Add(1)
		select {
		case seen <- r.URL.Query().Get("rid"):
		default:
		}
		_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	l := testLedger()
	l.origin = time.Now()
	l.cutoff = l.origin.Add(c.duration)
	l.seconds = make([]secondStats, 1)
	j := job{kind: "ok", rid: "1", scheduled: l.origin}
	w := testWorker(c, l)
	defer w.pool.close()
	done := make(chan struct{})
	l.mu.Lock()
	var unlock sync.Once
	release := func() { unlock.Do(l.mu.Unlock) }
	defer release()
	go func() { l.plan(j); w.execute(j); close(done) }()
	progress := false
	select {
	case rid := <-seen:
		progress = rid == j.rid
	case <-time.After(250 * time.Millisecond):
	}
	// Completion history may intentionally wait for this lock. Always release
	// it before joining, including on the old implementation's red path.
	release()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("worker did not retire after the historical lock was released")
	}
	w.pool.close()
	if requests.Load() != 1 || l.all.Planned != 1 || l.all.Attempted != 1 || l.all.Sent != 1 || l.all.Results["correct"] != 1 || l.active != 0 || l.live != 0 || l.dials != 1 {
		t.Fatalf("resource/history reconciliation changed: requests=%d stats=%+v active=%d live=%d dials=%d", requests.Load(), l.all, l.active, l.live, l.dials)
	}
	if !progress {
		t.Fatal("historical ledger lock prevented the real HTTP request from being sent")
	}
}

func TestWorkersFinishConstructionBeforeArrivalOrigin(t *testing.T) {
	c := testConfig(t)
	c.concurrency, c.queue = 8, 8
	var requests atomic.Uint32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests.Add(1)
		_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	l := testLedger()
	queue := make(chan job, c.queue)
	pool := newConnectionPool(c, l)
	var workers sync.WaitGroup
	entered := make(chan int, c.concurrency)
	release := make(chan struct{})
	var releaseOnce, closeQueueOnce sync.Once
	defer func() {
		releaseOnce.Do(func() { close(release) })
		closeQueueOnce.Do(func() { close(queue) })
		done := make(chan struct{})
		go func() { workers.Wait(); close(done) }()
		select {
		case <-done:
		case <-time.After(2 * time.Second):
			t.Error("request workers did not retire after queue close")
		}
		pool.close()
	}()
	var initialized atomic.Uint32
	type prepared struct {
		pool   workerPreparation
		origin time.Time
	}
	completed := make(chan prepared, 1)
	go func() {
		p := prepareRequestWorkers(c, l, pool, queue, &workers, func(index int) {
			if index == c.concurrency-1 {
				entered <- index
				<-release
				initialized.Add(1)
			} else {
				initialized.Add(1)
				entered <- index
			}
		})
		origin, _ := captureMonotonicAnchor(time.Now, readKernelMonotonic)
		completed <- prepared{p, origin}
	}()
	for i := 0; i < c.concurrency; i++ {
		select {
		case <-entered:
		case <-time.After(time.Second):
			t.Fatal("worker construction did not reach the controlled readiness boundary")
		}
	}
	// Hold one real worker before ready.Done. Merely launching all goroutines
	// (the previous origin boundary) must not publish an arrival origin yet.
	select {
	case <-completed:
		t.Fatal("origin established while a worker was still being constructed")
	case <-time.After(20 * time.Millisecond):
	}
	if initialized.Load() != uint32(c.concurrency-1) || requests.Load() != 0 {
		t.Fatalf("preparation crossed into business requests: ready=%d requests=%d", initialized.Load(), requests.Load())
	}
	releaseOnce.Do(func() { close(release) })
	var got prepared
	select {
	case got = <-completed:
	case <-time.After(time.Second):
		t.Fatal("all-ready worker pool did not release the arrival clock")
	}
	if initialized.Load() != uint32(c.concurrency) || got.pool.workers != c.concurrency || got.pool.ready.Before(got.pool.started) || got.pool.ready.After(got.origin) {
		t.Fatalf("worker readiness was not before the real origin: %+v initialized=%d", got, initialized.Load())
	}
	if l.dials != 0 || l.live != 0 || requests.Load() != 0 {
		t.Fatal("worker preparation prewarmed a connection or issued a request")
	}
	// Nothing is broadcast at origin. A ready worker responds to the first
	// actual queue item using its unchanged scheduled_at deadline.
	l.origin, l.cutoff = got.origin, got.origin.Add(c.duration)
	l.seconds = make([]secondStats, 1)
	j := job{kind: "ok", rid: "1", scheduled: got.origin}
	l.plan(j)
	enqueuePlanned(c, l, j, queue, time.Now())
	closeQueueOnce.Do(func() { close(queue) })
	workers.Wait()
	if requests.Load() != 1 || l.all.Planned != 1 || l.all.Results["correct"] != 1 || l.lastActualSent.Before(got.origin) || l.lastActualCompleted.Before(l.lastActualSent) {
		t.Fatalf("first original plan was not really sent after readiness: %+v", l.all)
	}
}

func TestPreparedTaskMixKeepsOriginalPlanAndClockLedger(t *testing.T) {
	c := taskMixTestConfig(t)
	c.duration, c.rate, c.cpuRate = 50*time.Millisecond, 200, 100
	c.concurrency, c.queue = 32, 32
	c.events = filepath.Join(t.TempDir(), "prepared-task-mix.csv.gz")
	type received struct {
		rid, path  string
		work, seed int
	}
	requests := make(chan received, 32)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		q := r.URL.Query()
		rid := q.Get("rid")
		if r.Header.Get("X-Request-ID") != rid || r.Header.Get("X-Cross-Stack-Request") != rid {
			t.Error("request identity changed after worker preparation")
		}
		requests <- received{rid, r.URL.Path, len(q.Get("work")), len(q.Get("seed"))}
		if r.URL.Path == c.protocol.Endpoints["cpu"].Path {
			_, _ = io.WriteString(w, rid+":"+c.checksums[fmt.Sprintf("%d/%d", len(q.Get("work")), len(q.Get("seed")))])
		} else {
			_, _ = io.WriteString(w, "ok:"+rid)
		}
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	report, err := run(c)
	if err != nil {
		t.Fatal(err)
	}
	descriptors := report["descriptor_preparation"].(map[string]any)
	if descriptors["configured_max_live_connections"] != c.concurrency || descriptors["complete_before_origin"] != true || descriptors["elapsed_ns"].(int64) < 0 || descriptors["completed_offset_ns"].(int64) > 0 || descriptors["network_prewarm"] != false {
		t.Fatalf("descriptor startup changed the arrival boundary: %#v", descriptors)
	}
	if descriptors["supported"] == true && (descriptors["dup_closed"] != true || descriptors["source_closed"] != true) {
		t.Fatalf("descriptor preparation left owned descriptors open: %#v", descriptors)
	}
	preparation := report["worker_preparation"].(map[string]any)
	if preparation["ready_workers"] != c.concurrency || preparation["configured_workers"] != c.concurrency || preparation["complete_before_origin"] != true || preparation["connections_dialed"] != uint64(0) || preparation["connections_live"] != uint64(0) || preparation["duration_ns"].(int64) < 0 || preparation["ready_offset_ns"].(int64) > 0 || preparation["origin_after_ready_ns"].(int64) < 0 {
		t.Fatalf("startup was not reported separately at the real readiness boundary: %#v", preparation)
	}
	file, err := os.Open(c.events)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	z, err := gzip.NewReader(file)
	if err != nil {
		t.Fatal(err)
	}
	defer z.Close()
	rows, err := csv.NewReader(z).ReadAll()
	if err != nil || len(rows) != 16 || len(rows[0]) != 21 {
		t.Fatalf("changed the finite plan or original event format: rows=%d error=%v", len(rows), err)
	}
	byRID := make(map[string][]string)
	var latestSent, latestCompleted int64
	for _, row := range rows[1:] {
		if _, duplicate := byRID[row[0]]; duplicate {
			t.Fatal("duplicate terminal for one plan", row)
		}
		byRID[row[0]] = row
		var scheduled, sent, completed int64
		if _, err := fmt.Sscan(row[3], &scheduled); err != nil {
			t.Fatal(err)
		}
		if _, err := fmt.Sscan(row[4], &sent); err != nil {
			t.Fatal(err)
		}
		if _, err := fmt.Sscan(row[5], &completed); err != nil {
			t.Fatal(err)
		}
		if row[7] != "correct" || row[19] != "true" || sent < scheduled || completed < sent || completed > scheduled+c.timeout.Nanoseconds() {
			t.Fatal("worker preparation changed a request deadline or terminal", row)
		}
		latestSent, latestCompleted = max(latestSent, sent), max(latestCompleted, completed)
	}
	observed := make(map[string]received)
	for len(requests) > 0 {
		r := <-requests
		observed[r.rid] = r
	}
	planOrigin := time.Unix(100, 0)
	plan := newTaskMixPlan(c, planOrigin)
	count := 0
	for j, ok := plan.next(); ok; j, ok = plan.next() {
		row, exists := byRID[j.rid]
		r, received := observed[j.rid]
		if !exists || !received || row[1] != j.kind || row[2] != fmt.Sprint(j.index) || row[3] != fmt.Sprint(j.scheduled.Sub(planOrigin).Nanoseconds()) || row[15] != fmt.Sprint(j.streamIndex) || row[16] != "false" || r.path != c.protocol.Endpoints[j.kind].Path {
			t.Fatalf("original independent plan, RID or order changed: job=%+v row=%v received=%+v", j, row, r)
		}
		if j.kind == "cpu" {
			work, seed := inputAt(c, j)
			if r.work != work || r.seed != seed {
				t.Fatal("CPU ordinal or seed changed", j, r)
			}
		}
		count++
	}
	ledger := report["ledger"].(map[string]any)
	clocks := report["configuration"].(map[string]any)
	if count != 15 || len(observed) != count || report["valid_driver"] != true || ledger["planned"] != uint64(count) || ledger["correct_total"] != uint64(count) || clocks["planned_cutoff_offset_ns"] != c.duration.Nanoseconds() || clocks["last_actual_sent_offset_ns"] != latestSent || clocks["last_actual_completed_offset_ns"] != latestCompleted || clocks["run_end_offset_ns"].(int64) < latestCompleted || clocks["timeout_seconds"] != c.timeout.Seconds() {
		t.Fatalf("preparation rewrote planned/actual clocks or hid a terminal: %#v", report)
	}
}

func TestSharedPoolExpiresBeforeNextPlan(t *testing.T) {
	c := testConfig(t)
	c.idle = 500 * time.Millisecond
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	w := testWorker(c, testLedger())
	defer w.pool.close()
	for i := 0; i < 2; i++ {
		j := job{kind: "ok", rid: fmt.Sprint(i + 1), scheduled: time.Now()}
		if r := w.request(j, j.scheduled); r.category != "correct" {
			t.Fatal(r)
		}
		if i == 0 {
			// Age the real returned socket deterministically; the frozen 500ms
			// policy is unchanged and the test need not sleep for that interval.
			w.pool.mu.Lock()
			w.pool.idle[0].last = time.Now().Add(-c.idle - time.Millisecond)
			w.pool.mu.Unlock()
		}
	}
	if w.l.dials != 2 || w.l.live != 1 {
		t.Fatalf("expired socket was reused or leaked: dials=%d live=%d", w.l.dials, w.l.live)
	}
	w.pool.close()
	if w.l.live != 0 {
		t.Fatal("expired-pool cleanup leaked a connection")
	}
}

func TestSharedPoolConcurrentRequestsRemainExclusive(t *testing.T) {
	c := testConfig(t)
	c.duration, c.rate, c.concurrency, c.idle = 50*time.Millisecond, 1000, 8, 500*time.Millisecond
	c.queue = 64
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(3 * time.Millisecond)
		_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	report, err := run(c)
	if err != nil {
		t.Fatal(err)
	}
	l, resources := report["ledger"].(map[string]any), report["resources"].(map[string]any)
	if report["valid_driver"] != true || l["correct_total"] != uint64(50) || resources["dials"].(uint64) > uint64(c.concurrency) || resources["peak_connections"].(uint64) > uint64(c.concurrency) || resources["connections_after_cleanup"] != uint64(0) {
		t.Fatalf("concurrent checkouts interleaved requests or leaked sockets: %#v", report)
	}
}

type failingWriteConn struct {
	net.Conn
	positive bool
}

func (c failingWriteConn) Write(p []byte) (int, error) {
	if c.positive && len(p) > 0 {
		return 1, io.ErrClosedPipe
	}
	return 0, io.ErrClosedPipe
}

func TestReusedFailedWriteHasOnlyCurrentPlanBytesAndClock(t *testing.T) {
	for _, positive := range []bool{false, true} {
		t.Run(fmt.Sprint(positive), func(t *testing.T) {
			c := testConfig(t)
			c.address = "127.0.0.1:1"
			w := testWorker(c, testLedger())
			defer w.pool.close()
			local, peer := net.Pipe()
			defer peer.Close()
			conn := &countedConn{Conn: failingWriteConn{local, positive}, written: 123, first: time.Now().Add(-time.Second)}
			w.l.live = 1
			w.pool.put(&idleConnection{conn: conn, reader: bufio.NewReader(conn), last: time.Now()})
			j := job{kind: "ok", rid: "1", scheduled: time.Now()}
			r := w.request(j, j.scheduled)
			if r.category != "connection_error" || r.stage != "write" || w.l.dials != 0 || w.l.live != 0 || len(w.pool.idle) != 0 {
				t.Fatalf("failed pooled write retained/replayed a connection: %+v", r)
			}
			if positive {
				if r.bytesWritten != 1 || r.sent.Before(j.scheduled) {
					t.Fatalf("partial write used prior plan bytes/clock: %+v", r)
				}
			} else if r.bytesWritten != 0 || !r.sent.IsZero() {
				t.Fatalf("zero-byte write inherited prior plan bytes/clock: %+v", r)
			}
		})
	}
}

func TestSharedPoolLIFOBoundAndClose(t *testing.T) {
	c := testConfig(t)
	c.concurrency = 2
	l := testLedger()
	p := newConnectionPool(c, l)
	defer p.close()
	connections := make([]*idleConnection, 3)
	for i := range connections {
		local, peer := net.Pipe()
		t.Cleanup(func() { _ = peer.Close() })
		conn := &countedConn{Conn: local}
		connections[i] = &idleConnection{conn: conn, reader: bufio.NewReader(conn), last: time.Now()}
		l.live++
		p.put(connections[i])
	}
	if len(p.idle) != 2 || l.live != 2 {
		t.Fatalf("idle capacity was not bounded: idle=%d live=%d", len(p.idle), l.live)
	}
	borrowed, closed := p.take()
	if closed || borrowed != connections[1] {
		t.Fatal("pool did not return the most recently returned connection")
	}
	p.close()
	if l.live != 1 {
		t.Fatal("pool close affected a still-owned request connection")
	}
	// Production closes after workers finish; a late return is also discarded.
	p.put(borrowed)
	p.close()
	if l.live != 0 || len(p.idle) != 0 {
		t.Fatalf("pool close/late return leaked sockets: live=%d idle=%d", l.live, len(p.idle))
	}
	if conn, closed := p.take(); conn != nil || !closed {
		t.Fatal("closed pool allowed another checkout")
	}
}

func TestReusedConnectionFailuresNeverReplay(t *testing.T) {
	for _, scenario := range []string{"io", "protocol", "body", "timeout", "close", "rejected", "rate_limited"} {
		t.Run(scenario, func(t *testing.T) {
			c := testConfig(t)
			c.idle, c.timeout = 500*time.Millisecond, 150*time.Millisecond
			var arrivals atomic.Uint32
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				arrivals.Add(1)
				if r.URL.Query().Get("rid") == "2" {
					switch scenario {
					case "io", "protocol":
						conn, _, err := w.(http.Hijacker).Hijack()
						if err != nil {
							t.Error(err)
							return
						}
						if scenario == "protocol" {
							_, _ = io.WriteString(conn, "not HTTP\r\n\r\n")
						}
						_ = conn.Close()
						return
					case "body":
						_, _ = io.WriteString(w, "wrong body")
						return
					case "timeout":
						<-r.Context().Done()
						return
					case "close":
						w.Header().Set("Connection", "close")
					case "rejected", "rate_limited":
						status := http.StatusServiceUnavailable
						if scenario == "rate_limited" {
							status = http.StatusTooManyRequests
						}
						w.WriteHeader(status)
						_, _ = io.WriteString(w, "database_error:pool_busy")
						return
					}
				}
				_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
			}))
			defer server.Close()
			c.address = strings.TrimPrefix(server.URL, "http://")
			l := testLedger()
			pool := newConnectionPool(c, l)
			defer pool.close()
			workers := []worker{{c: c, l: l, pool: pool}, {c: c, l: l, pool: pool}}
			request := func(index int, rid string) result {
				j := job{kind: "ok", rid: rid, scheduled: time.Now()}
				return workers[index].request(j, j.scheduled)
			}
			if r := request(0, "1"); r.category != "correct" {
				t.Fatal(r)
			}
			r := request(1, "2")
			expected := map[string]string{"io": "connection_error", "protocol": "protocol_error", "body": "body_error", "timeout": "timeout", "close": "correct", "rejected": "rejected_503", "rate_limited": "rejected_429"}[scenario]
			if r.category != expected || arrivals.Load() != 2 || l.dials != 1 || r.sent.IsZero() {
				t.Fatalf("failed reused request was replayed or misclassified: %+v arrivals=%d dials=%d", r, arrivals.Load(), l.dials)
			}
			live := uint64(0)
			if scenario == "rejected" || scenario == "rate_limited" {
				live = 1
				if r.detail != "database_error:pool_busy" {
					t.Fatalf("bounded rejection lost its error category: %+v", r)
				}
			}
			if l.live != live || uint64(len(pool.idle)) != live {
				t.Fatalf("bad connection was retained: live=%d idle=%d", l.live, len(pool.idle))
			}
			if r := request(0, "3"); r.category != "correct" || arrivals.Load() != 3 || l.dials != 2-live {
				t.Fatalf("new distinct plan failed: %+v arrivals=%d dials=%d", r, arrivals.Load(), l.dials)
			}
			pool.close()
			if l.live != 0 {
				t.Fatal("failure cleanup leaked a connection")
			}
		})
	}
}

func TestRawPOSTWritesFrozenCorpusAndChecksReturnedData(t *testing.T) {
	c := testConfig(t)
	for _, name := range []string{"object", "array", "escape"} {
		t.Run(name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				body, e := io.ReadAll(r.Body)
				if e != nil || string(body) != string(c.corpora[name].bytes) || r.Method != "POST" || r.ContentLength != int64(len(body)) || r.Header.Get("Accept-Encoding") != "identity" {
					t.Errorf("wrong request method/body/framing: %s %d %v", r.Method, r.ContentLength, e)
					w.WriteHeader(400)
					return
				}
				input, e := decode(body)
				if e != nil {
					t.Error(e)
					w.WriteHeader(400)
					return
				}
				object := input.(map[string]any)
				_ = json.NewEncoder(w).Encode(map[string]any{"id": r.URL.Query().Get("rid"), "counter": 8, "data": object["data"]})
			}))
			defer server.Close()
			c.address = strings.TrimPrefix(server.URL, "http://")
			worker := testWorker(c, testLedger())
			defer worker.pool.close()
			j := job{kind: "json_" + name, rid: "1234567890000001", scheduled: time.Now()}
			r := worker.request(j, j.scheduled)
			if r.category != "correct" || !r.writeComplete || r.bytesWritten <= int64(len(c.corpora[name].bytes)) || r.bytesRead == 0 {
				t.Fatalf("wrong complete POST result: %+v", r)
			}
		})
	}
}

func TestSlowDripUsesOneAbsoluteDeadline(t *testing.T) {
	c := testConfig(t)
	c.timeout = 35 * time.Millisecond
	listener, e := net.Listen("tcp", "127.0.0.1:0")
	if e != nil {
		t.Fatal(e)
	}
	defer listener.Close()
	c.address = listener.Addr().String()
	done := make(chan struct{})
	go func() {
		defer close(done)
		conn, e := listener.Accept()
		if e != nil {
			return
		}
		defer conn.Close()
		_, e = http.ReadRequest(bufio.NewReader(conn))
		if e != nil {
			return
		}
		for _, ch := range []byte("HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nok:1") {
			if _, e = conn.Write([]byte{ch}); e != nil {
				return
			}
			time.Sleep(5 * time.Millisecond)
		}
	}()
	w := testWorker(c, testLedger())
	j := job{kind: "ok", rid: "1", scheduled: time.Now()}
	result := w.request(j, j.scheduled)
	elapsed := time.Since(j.scheduled)
	w.pool.close()
	<-done
	if result.category != "timeout" || elapsed > 200*time.Millisecond {
		t.Fatalf("deadline renewed: %+v elapsed=%v", result, elapsed)
	}
}

func TestLateWindowCompletionAndBoundedCSVLedger(t *testing.T) {
	c := testConfig(t)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(70 * time.Millisecond)
		_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	c.events = filepath.Join(t.TempDir(), "clock.csv.gz")
	report, e := run(c)
	if e != nil {
		t.Fatal(e)
	}
	ledger := report["ledger"].(map[string]any)
	if report["valid_driver"] != true || ledger["planned"] != uint64(1) || ledger["correct_total"] != uint64(1) || ledger["correct_in_window"] != uint64(0) || ledger["goodput_rps"] != float64(0) {
		t.Fatalf("late completion changed measurement window: %#v", report)
	}
	file, e := os.Open(c.events)
	if e != nil {
		t.Fatal(e)
	}
	defer file.Close()
	gz, e := gzip.NewReader(file)
	if e != nil {
		t.Fatal(e)
	}
	defer gz.Close()
	rows, e := csv.NewReader(gz).ReadAll()
	if e != nil {
		t.Fatal(e)
	}
	if len(rows) != 2 || len(rows[0]) != 15 || rows[1][7] != "correct" || rows[1][3] == "" || rows[1][4] == "" || rows[1][5] == "" {
		t.Fatal(rows)
	}
	if _, e = openEvents(c.events); e == nil {
		t.Fatal("overwrote retained evidence")
	}
}

func TestQueuedLastPlanStillSendsAfterWindowWithinOriginalDeadline(t *testing.T) {
	c := testConfig(t)
	c.duration, c.rate, c.concurrency = 20*time.Millisecond, 100, 1
	firstEntered, releaseFirst := make(chan struct{}), make(chan struct{})
	var arrivals atomic.Uint32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if arrivals.Add(1) == 1 {
			close(firstEntered)
			<-releaseFirst
		}
		_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	type outcome struct {
		report map[string]any
		err    error
	}
	done := make(chan outcome, 1)
	go func() { report, err := run(c); done <- outcome{report, err} }()
	select {
	case <-firstEntered:
	case <-time.After(time.Second):
		close(releaseFirst)
		t.Fatal("first planned request never reached the server")
	}
	// The second plan belongs to the original 20ms window but cannot leave
	// the single worker until after that window. Its own 1s deadline remains.
	time.Sleep(2 * c.duration)
	close(releaseFirst)
	var observed outcome
	select {
	case observed = <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("planned tail did not terminate within its original deadline")
	}
	if observed.err != nil {
		t.Fatal(observed.err)
	}
	l := observed.report["ledger"].(map[string]any)
	if observed.report["valid_driver"] != true || arrivals.Load() != 2 || l["planned"] != uint64(2) || l["sent"] != uint64(2) || l["correct_total"] != uint64(2) || l["correct_in_window"] != uint64(0) || l["goodput_rps"] != float64(0) {
		t.Fatalf("window cutoff discarded a still-live plan or backfilled goodput: %#v", observed.report)
	}
	clocks := observed.report["configuration"].(map[string]any)
	if clocks["planned_cutoff_offset_ns"] != c.duration.Nanoseconds() || clocks["last_actual_sent_offset_ns"].(int64) <= c.duration.Nanoseconds() || clocks["last_actual_completed_offset_ns"].(int64) < clocks["last_actual_sent_offset_ns"].(int64) {
		t.Fatalf("actual tail clocks lost their original window: %#v", clocks)
	}
	if clocks["last_actual_sent_rid"] != "0000000010000001" || clocks["last_actual_sent_unix_ns"] == nil || clocks["last_complete_response_offset_ns"] == nil {
		t.Fatal(clocks)
	}
}

func TestSchedulerAdmitsLivePlanAfterCutoff(t *testing.T) {
	c := testConfig(t)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	l := testLedger()
	l.origin = time.Now().Add(-40 * time.Millisecond)
	l.cutoff = l.origin.Add(c.duration)
	l.seconds = make([]secondStats, 1)
	j := job{kind: "ok", rid: "1", scheduled: l.cutoff.Add(-time.Millisecond)}
	l.plan(j)
	queue := make(chan job, 1)
	enqueuePlanned(c, l, j, queue, time.Now())
	if len(queue) != 1 {
		t.Fatalf("scheduler discarded a plan before scheduled+timeout: %#v", l.all.Results)
	}
	w := testWorker(c, l)
	w.execute(<-queue)
	w.pool.close()
	if l.all.Results["correct"] != 1 || l.all.Sent != 1 || l.all.CorrectWindow != 0 || !l.lastActualSent.After(l.cutoff) {
		t.Fatalf("admitted tail was not really sent: %#v", l.all)
	}
}

func TestExpiredPlanNeverDialsOrRenewsDeadline(t *testing.T) {
	c := testConfig(t)
	listener, err := net.ListenTCP("tcp", &net.TCPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	c.address = listener.Addr().String()
	makeExpired := func() (*ledger, job) {
		l := testLedger()
		l.origin = time.Now().Add(-2 * c.timeout)
		l.cutoff = l.origin.Add(c.duration)
		l.seconds = make([]secondStats, 1)
		j := job{kind: "ok", rid: "1", scheduled: l.origin}
		l.plan(j)
		return l, j
	}
	l, j := makeExpired()
	queue := make(chan job, 1)
	enqueuePlanned(c, l, j, queue, time.Now())
	if len(queue) != 0 || l.all.Results["not_started_scheduler_deadline"] != 1 || l.dials != 0 {
		t.Fatalf("expired scheduler plan was admitted: %#v", l.all)
	}
	l, j = makeExpired()
	w := testWorker(c, l)
	w.execute(j)
	if l.all.Results["not_started_deadline"] != 1 || l.all.Attempted != 0 || l.dials != 0 {
		t.Fatalf("expired worker plan created an attempt: %#v", l.all)
	}
	// Also exercise the raw request guard directly: a fresh began timestamp
	// cannot replace the expired scheduled_at deadline.
	r := w.request(j, time.Now())
	w.pool.close()
	if r.category != "not_started_deadline" || !r.sent.IsZero() || l.dials != 0 || !l.lastActualSent.IsZero() || unixNano(l.lastActualSent) != nil {
		t.Fatalf("raw request renewed an expired plan: %+v", r)
	}
	_ = listener.SetDeadline(time.Now().Add(20 * time.Millisecond))
	if conn, err := listener.Accept(); err == nil {
		conn.Close()
		t.Fatal("an expired plan created a socket or replay")
	} else if timeout, ok := err.(net.Error); !ok || !timeout.Timeout() {
		t.Fatal(err)
	}
}

func TestDriverQueueLossHistogramAndEventOverflowAreExplicit(t *testing.T) {
	c := testConfig(t)
	c.duration = 40 * time.Millisecond
	c.rate = 5000
	c.queue = 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(15 * time.Millisecond)
		_, _ = io.WriteString(w, "ok:"+r.URL.Query().Get("rid"))
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	report, e := run(c)
	if e != nil {
		t.Fatal(e)
	}
	if report["valid_driver"] != false {
		t.Fatal("queue loss did not invalidate capacity observation")
	}
	ledger := report["ledger"].(map[string]any)
	if ledger["planned_reconciled"] != true {
		t.Fatal(ledger)
	}
	h := histogram{}
	budget := histogramBudget{keys: maxHistogramKeys}
	h.add(time.Second, &budget)
	if budget.overflow || h.count != 1 {
		t.Fatal("fixed histogram lost a sample after historical sparse-key bound")
	}
	w := eventWriter{queue: make(chan event, 1)}
	w.offer(event{})
	w.offer(event{})
	if w.dropped != 1 {
		t.Fatal("event overflow silently blocked/lost record")
	}
}

func TestFixedHistogramPreservesOverMillionDistinctDurations(t *testing.T) {
	h := histogram{}
	budget := histogramBudget{}
	for i := 0; i < 1200000; i++ {
		h.add(time.Duration(i)*time.Microsecond*10, &budget)
	}
	h.add(time.Duration(math.MaxInt64), &budget)
	h.add(-time.Second, &budget)
	var sum uint64
	for _, n := range h.bins {
		sum += n
	}
	if h.count != 1200002 || sum != h.count || budget.overflow || len(h.bins) != maxHistogramKeys {
		t.Fatalf("lost counts or unbounded cells: %d %d %d", h.count, sum, len(h.bins))
	}
	for _, n := range []int64{0, 1, 9999, 10000, 2559999, 2560000, 100000000, 3000000000, math.MaxInt64} {
		index := histogramIndex(n)
		if index < 0 || index >= maxHistogramKeys || histogramUpper(index) < n {
			t.Fatalf("duration not conservatively represented: %d %d %d", n, index, histogramUpper(index))
		}
	}
}
func TestTailSamplesAndIdleDiagnosticCounters(t *testing.T) {
	for _, count := range []int{0, 99, 100, 999, 1000} {
		h := histogram{}
		budget := histogramBudget{}
		for i := 0; i < count; i++ {
			h.add(time.Millisecond, &budget)
		}
		v := h.value()
		if (v["p99_ms"] != nil) != (count >= 100) || (v["p999_ms"] != nil) != (count >= 1000) {
			t.Fatal(count, v)
		}
	}
}

func taskMixTestConfig(t *testing.T) config {
	c := testConfig(t)
	c.workload, c.duration, c.rate, c.cpuRate = "task_mix", time.Second, 4, 2
	c.taskMixShortSLOMS, c.cpuCancelDelay = 20, 250*time.Microsecond
	e := c.protocol.Endpoints["cpu"]
	e.Work, e.Seeds = []int{512}, []int{3, 11}
	c.protocol.Endpoints["cpu"] = e
	if err := validateTaskMix(c); err != nil {
		t.Fatal(err)
	}
	return c
}

func TestTaskMixPlansIndependentStreamsAndOriginalInputs(t *testing.T) {
	c := taskMixTestConfig(t)
	origin := time.Unix(100, 0)
	plan := newTaskMixPlan(c, origin)
	wantKinds := []string{"ok", "cpu", "ok", "ok", "cpu", "ok"}
	wantOffsets := []time.Duration{0, 0, 250 * time.Millisecond, 500 * time.Millisecond, 500 * time.Millisecond, 750 * time.Millisecond}
	var cpuJobs []job
	for index := range wantKinds {
		j, ok := plan.next()
		if !ok || j.index != uint64(index) || j.kind != wantKinds[index] || j.scheduled.Sub(origin) != wantOffsets[index] {
			t.Fatalf("plan %d: %+v", index, j)
		}
		if j.kind == "cpu" {
			units, seed := inputAt(c, j)
			if units != 512 || seed != []int{3, 11}[len(cpuJobs)] {
				t.Fatalf("CPU ordinal lost: %+v input=%d/%d", j, units, seed)
			}
			cpuJobs = append(cpuJobs, j)
		}
	}
	if _, ok := plan.next(); ok {
		t.Fatal("extra plan")
	}
	// Changing short traffic must not change CPU timestamps, ordinal or inputs.
	c.rate = 9
	plan = newTaskMixPlan(c, origin)
	count := 0
	for j, ok := plan.next(); ok; j, ok = plan.next() {
		if j.kind == "cpu" {
			if j.streamIndex != cpuJobs[count].streamIndex || j.scheduled != cpuJobs[count].scheduled {
				t.Fatal("CPU stream depended on short arrival rate")
			}
			count++
		}
	}
	if count != 2 {
		t.Fatal(count)
	}
	// The original per-workload index selection and70/20/9/1 mixture stay intact.
	c.workload = "cpu"
	e := c.protocol.Endpoints["cpu"]
	e.Work, e.Seeds = []int{480, 512, 544, 576}, []int{1, 3, 7, 11}
	c.protocol.Endpoints["cpu"] = e
	if units, seed := inputAt(c, job{index: 5, streamIndex: 0}); units != 512 || seed != 3 {
		t.Fatal("old CPU selection changed", units, seed)
	}
}

func TestTaskMixCancellationPlanAndNormalDenominators(t *testing.T) {
	c := taskMixTestConfig(t)
	c.rate, c.cpuRate, c.cpuCancelEvery = 17, 20, 10
	origin := time.Unix(100, 0)
	plan := newTaskMixPlan(c, origin)
	l := &ledger{all: newStats(), types: map[string]*stats{}, seconds: make([]secondStats, 1), origin: origin, cutoff: origin.Add(time.Second)}
	seen := map[string]bool{}
	counts := map[string]int{}
	var resets []uint64
	for j, ok := plan.next(); ok; j, ok = plan.next() {
		if seen[j.rid] {
			t.Fatal("duplicate merged RID", j.rid)
		}
		seen[j.rid] = true
		counts[j.kind]++
		l.plan(j)
		l.start()
		r := result{category: "correct", sent: j.scheduled.Add(time.Microsecond), completed: j.scheduled.Add(time.Millisecond), responseComplete: true}
		if j.cpuCancel {
			resets = append(resets, j.streamIndex)
			r.category, r.responseComplete = "planned_reset", false
			r.cancelDue, r.cancelIssued = r.sent.Add(c.cpuCancelDelay), r.sent.Add(c.cpuCancelDelay+time.Microsecond)
		}
		l.finish(j, j.scheduled, r, c)
	}
	if counts["ok"] != 17 || counts["cpu"] != 20 || len(resets) != 2 || resets[0] != 9 || resets[1] != 19 {
		t.Fatal(counts, resets)
	}
	view := l.types["cpu"].taskMixView(1, 1000)
	if view["planned"] != uint64(20) || view["normal_planned"] != uint64(18) || view["normal_correct"] != uint64(18) || view["normal_failure_fraction_planned"] != float64(0) || l.types["cpu"].Results["planned_reset"] != 2 || len(l.samples) != 0 || len(l.cancelSamples) != 2 {
		t.Fatal(view, l.samples, l.cancelSamples)
	}
	// A predesignated but unsent cancellation remains visible; it is not a
	// successful reset, and run's original not_started gate still invalidates it.
	j := job{kind: "cpu", cpuCancel: true, streamIndex: 29, rid: "unsent", scheduled: origin}
	l.plan(j)
	l.finish(j, time.Time{}, result{category: "not_started_queue", completed: origin.Add(time.Second)}, c)
	view = l.types["cpu"].taskMixView(1, 1000)
	cancel := view["planned_cancellation"].(map[string]any)
	if view["normal_planned"] != uint64(18) || cancel["planned"] != uint64(3) || cancel["reset_issued"] != uint64(2) || len(l.samples) != 1 || l.all.Results["not_started_queue"] != 1 {
		t.Fatal(view, l.samples)
	}
	path := filepath.Join(t.TempDir(), "task-mix.csv.gz")
	events, err := openEvents(path, true)
	if err != nil {
		t.Fatal(err)
	}
	events.offer(event{j: job{kind: "cpu", rid: "reset", streamIndex: 9, cpuCancel: true, scheduled: origin, generated: origin.Add(100 * time.Microsecond)}, origin: origin, result: result{category: "planned_reset", sent: origin, completed: origin.Add(time.Millisecond), cancelDue: origin.Add(250 * time.Microsecond), cancelIssued: origin.Add(300 * time.Microsecond), writeComplete: true}})
	events.close()
	file, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	z, err := gzip.NewReader(file)
	if err != nil {
		t.Fatal(err)
	}
	defer z.Close()
	rows, err := csv.NewReader(z).ReadAll()
	if err != nil || len(rows) != 2 || len(rows[0]) != 21 || rows[0][20] != "generated_ns" || rows[1][20] != "100000" || rows[1][7] != "planned_reset" || rows[1][15] != "9" || rows[1][16] != "true" || rows[1][17] != "250000" || rows[1][18] != "300000" || rows[1][19] != "true" {
		t.Fatal(rows, err)
	}
}

func TestTaskMixRejectsUnfrozenProtocolAndBadBounds(t *testing.T) {
	c := testConfig(t)
	c.workload, c.cpuRate, c.taskMixShortSLOMS = "task_mix", 1, 20
	if validateTaskMix(c) == nil {
		t.Fatal("accepted original multi-input protocol as frozen task_mix")
	}
	c = taskMixTestConfig(t)
	c.cpuRate = math.NaN()
	if validateTaskMix(c) == nil {
		t.Fatal("accepted NaN CPU rate")
	}
	c = taskMixTestConfig(t)
	c.cpuCancelEvery, c.cpuCancelDelay = 10, c.timeout
	if validateTaskMix(c) == nil {
		t.Fatal("renewed original deadline for reset")
	}
}

func TestTaskMixResetRetiresExactlyOneTCPLease(t *testing.T) {
	c := taskMixTestConfig(t)
	listener, err := net.ListenTCP("tcp", &net.TCPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	c.address = listener.Addr().String()
	done := make(chan error, 1)
	go func() {
		_ = listener.SetDeadline(time.Now().Add(time.Second))
		conn, err := listener.AcceptTCP()
		if err != nil {
			done <- err
			return
		}
		defer conn.Close()
		_ = conn.SetDeadline(time.Now().Add(time.Second))
		req, err := http.ReadRequest(bufio.NewReader(conn))
		if err != nil {
			if timeout, ok := err.(net.Error); ok && timeout.Timeout() {
				done <- err
			} else {
				// The reset may discard data before the peer parsed HTTP.
				done <- nil
			}
			return
		}
		_ = req.Body.Close()
		if req.URL.Query().Get("rid") != "reset" || len(req.URL.Query().Get("work")) != 512 || len(req.URL.Query().Get("seed")) != 11 {
			done <- fmt.Errorf("unexpected reset request")
			return
		}
		var probe [1]byte
		_, err = conn.Read(probe[:])
		if err == nil {
			err = fmt.Errorf("request replayed on reset socket")
		} else if timeout, ok := err.(net.Error); ok && timeout.Timeout() {
			// A peer that merely reached its deadline did not observe closure.
		} else {
			err = nil // peer stream ended; wire RST receipt itself is not asserted
		}
		done <- err
	}()
	l := testLedger()
	w := testWorker(c, l)
	j := job{kind: "cpu", streamIndex: 9, cpuCancel: true, rid: "reset", scheduled: time.Now()}
	r := w.request(j, j.scheduled)
	w.pool.close()
	if r.category != "planned_reset" || !r.writeComplete || r.sent.IsZero() || r.cancelIssued.IsZero() || r.cancelDue.Sub(r.sent) != c.cpuCancelDelay || r.cancelIssued.Before(r.cancelDue) || r.responseComplete || l.live != 0 || l.dials != 1 || len(w.pool.idle) != 0 {
		t.Fatalf("reset changed ownership/accounting: %+v live=%d dials=%d", r, l.live, l.dials)
	}
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("owned reset peer failed to exit")
	}
}

func TestConstantHTTPDiagnosticRequiresBoundedPairedOverrides(t *testing.T) {
	base := testConfig(t)
	if err := validateConstantDiagnostic(base); err != nil {
		t.Fatal(err)
	}
	constant := base
	constant.responseLiteralSet, constant.pathOverrideSet = true, true
	constant.responseLiteral, constant.pathOverride = "h4:constant", "/constant"
	for _, path := range []string{"/", "/constant", "/escaped%20path", "/%E4%B8%AD%E6%96%87", "/" + strings.Repeat("a", maxDiagnosticPath-1)} {
		c := constant
		c.pathOverride = path
		if err := validateConstantDiagnostic(c); err != nil {
			t.Fatalf("valid path %q: %v", path, err)
		}
	}
	for _, path := range []string{"", "constant", "http://example.test/constant", "//example.test/constant", "/constant?x=1", "/constant#fragment", "/with space", "/bad\r\nheader", "/bad%escape", "/" + strings.Repeat("a", maxDiagnosticPath)} {
		c := constant
		c.pathOverride = path
		if err := validateConstantDiagnostic(c); err == nil {
			t.Fatalf("accepted invalid path %q", path)
		}
	}
	for _, workload := range []string{"mixed", "task_mix"} {
		c := constant
		c.workload = workload
		if err := validateConstantDiagnostic(c); err == nil {
			t.Fatalf("accepted mixed oracle override %s", workload)
		}
	}
	for _, onlyResponse := range []bool{false, true} {
		c := constant
		c.responseLiteralSet, c.pathOverrideSet = onlyResponse, !onlyResponse
		if err := validateConstantDiagnostic(c); err == nil {
			t.Fatal("accepted an unpaired diagnostic override")
		}
	}
	c := constant
	c.responseLiteral = strings.Repeat("a", maxBody)
	if err := validateConstantDiagnostic(c); err != nil {
		t.Fatal(err)
	}
	c.responseLiteral += "a"
	if err := validateConstantDiagnostic(c); err == nil {
		t.Fatal("accepted an oversized response literal")
	}
	j := job{kind: "ok", rid: "0000000010000003"}
	method, path, body := requestSpec(constant, j)
	if method != "GET" || path != "/constant?rid="+j.rid || body != nil {
		t.Fatalf("diagnostic request lost its original method/RID: %s %s %q", method, path, body)
	}
	if err := validate(constant, j, []byte(constant.responseLiteral)); err != nil {
		t.Fatal(err)
	}
	if err := validate(constant, j, []byte(constant.responseLiteral+"garbage")); err == nil {
		t.Fatal("accepted a prefix instead of the complete fixed body")
	}
	if err := validate(constant, job{kind: "cpu", rid: j.rid}, []byte(constant.responseLiteral)); err == nil {
		t.Fatal("constant oracle accepted another stream")
	}
	constant.responseLiteral = ""
	if err := validateConstantDiagnostic(constant); err != nil {
		t.Fatal("explicit empty response literal must be supported", err)
	}
	if err := validate(constant, j, nil); err != nil {
		t.Fatal(err)
	}
	if err := validate(constant, j, []byte("x")); err == nil {
		t.Fatal("empty literal accepted a nonempty response")
	}
	method, path, _ = requestSpec(base, j)
	if method != "GET" || path != "/ok?rid="+j.rid {
		t.Fatal("default request path changed")
	}
	if err := validate(base, j, []byte("h4:constant")); err == nil {
		t.Fatal("default body oracle was weakened")
	}
}

func TestConstantHTTPDiagnosticUsesFullBodyOracleAndExplicitReport(t *testing.T) {
	c := testConfig(t)
	c.responseLiteralSet, c.pathOverrideSet = true, true
	c.pathOverride, c.responseLiteral = "/constant", "h4:constant"
	var requests atomic.Uint32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests.Add(1)
		if r.URL.Path != "/constant" || r.URL.Query().Get("rid") == "" || r.Header.Get("X-Request-ID") != r.URL.Query().Get("rid") {
			t.Error("diagnostic path/RID transport differs")
		}
		_, _ = io.WriteString(w, "h4:constant")
	}))
	defer server.Close()
	c.address = strings.TrimPrefix(server.URL, "http://")
	report, err := run(c)
	if err != nil {
		t.Fatal(err)
	}
	if requests.Load() != 1 || report["diagnostic_only"] != true || report["normal_score_eligible"] != false {
		t.Fatalf("missing diagnostic ledger/eligibility: %+v", report)
	}
	ledger := report["ledger"].(map[string]any)
	if ledger["correct_total"] != uint64(1) {
		t.Fatalf("fixed correct body was not counted: %+v", ledger)
	}
	configuration := report["configuration"].(map[string]any)
	oracle := configuration["constant_http_diagnostic"].(map[string]any)
	if oracle["path_override"] != "/constant" || oracle["response_literal_bytes"] != len(c.responseLiteral) || len(oracle["response_literal_sha256"].(string)) != 64 || configuration["no_retry"] != true {
		t.Fatalf("wrong bounded diagnostic identity: %+v", configuration)
	}
	// A successful HTTP200 with the wrong complete body remains an error, and
	// it cannot be replayed as another request through the shared pool.
	c.responseLiteral = "different"
	w := testWorker(c, testLedger())
	j := job{kind: "ok", rid: "1", scheduled: time.Now()}
	result := w.request(j, j.scheduled)
	w.pool.close()
	if result.category != "body_error" || result.stage != "validate" || requests.Load() != 2 {
		t.Fatalf("wrong diagnostic body was accepted/retried: %+v", result)
	}
}
