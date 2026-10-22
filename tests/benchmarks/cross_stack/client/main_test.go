package main

import (
	"bufio"
	"compress/gzip"
	"encoding/csv"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
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
	if report["schema"] != "ku-cross-stack-client-v4" || report["valid_driver"] != true || resources["dials"] != uint64(1) || resources["connections_after_cleanup"] != uint64(0) || resources["active_after_cleanup"] != uint64(0) {
		t.Fatalf("run path did not share/close idle connections: %#v", report)
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
	for _, scenario := range []string{"io", "protocol", "body", "timeout", "close", "rejected"} {
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
					case "rejected":
						w.WriteHeader(http.StatusServiceUnavailable)
						_, _ = io.WriteString(w, "busy")
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
			expected := map[string]string{"io": "connection_error", "protocol": "protocol_error", "body": "body_error", "timeout": "timeout", "close": "correct", "rejected": "rejected_503"}[scenario]
			if r.category != expected || arrivals.Load() != 2 || l.dials != 1 || r.sent.IsZero() {
				t.Fatalf("failed reused request was replayed or misclassified: %+v arrivals=%d dials=%d", r, arrivals.Load(), l.dials)
			}
			live := uint64(0)
			if scenario == "rejected" {
				live = 1
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
	if len(rows) != 2 || rows[1][7] != "correct" || rows[1][3] == "" || rows[1][4] == "" || rows[1][5] == "" {
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
	if !budget.overflow || h.count != 0 {
		t.Fatal("histogram budget silently lost sample")
	}
	w := eventWriter{queue: make(chan event, 1)}
	w.offer(event{})
	w.offer(event{})
	if w.dropped != 1 {
		t.Fatal("event overflow silently blocked/lost record")
	}
}
