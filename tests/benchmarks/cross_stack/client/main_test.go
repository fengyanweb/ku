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
	w := worker{c: c, l: testLedger()}
	j := job{kind: "ok", rid: "1", scheduled: time.Now()}
	result := w.request(j, j.scheduled)
	w.close()
	if result.category != "connection_error" || result.sent.IsZero() {
		t.Fatalf("wrong failure classification: %+v", result)
	}
	<-done
	if accepted.Load() != 1 {
		t.Fatalf("failed request replayed %d times", accepted.Load())
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
			worker := worker{c: c, l: testLedger()}
			defer worker.close()
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
	w := worker{c: c, l: testLedger()}
	j := job{kind: "ok", rid: "1", scheduled: time.Now()}
	result := w.request(j, j.scheduled)
	elapsed := time.Since(j.scheduled)
	w.close()
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
