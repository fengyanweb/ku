// Full-stack comparison: standard net/http, encoding/json and normal Go GC.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"runtime"
	"runtime/metrics"
	"runtime/pprof"
	"strconv"
	"strings"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

type cpuJob struct {
	iterations int64
	seed       int64
	reply      chan int64
	context    context.Context
}

func checksum(iterations, seed int64) int64 {
	total := seed
	for i := int64(0); i < iterations; i++ {
		total = (total*33 + i + seed) % 100000000
	}
	return total
}

func envInt(name string, fallback int) int {
	text := os.Getenv(name)
	if text == "" {
		return fallback
	}
	value, err := strconv.Atoi(text)
	if err != nil || value < 1 {
		log.Fatalf("invalid %s", name)
	}
	return value
}

func batch() error {
	body, err := os.ReadFile("batch.json")
	if err != nil {
		return err
	}
	var input struct {
		Iterations int64 `json:"iterations"`
		Seed       int64 `json:"seed"`
		Repeat     int   `json:"repeat"`
	}
	if err := json.Unmarshal(body, &input); err != nil {
		return err
	}
	if input.Iterations < 1000 || input.Iterations > 1000000 || input.Seed < 1 || input.Seed > 1024 || input.Repeat < 0 || input.Repeat > 32768 {
		return errors.New("CPU fixture input outside common integer range")
	}
	checksums := make([]int64, input.Repeat)
	began := time.Now()
	for i := range checksums {
		checksums[i] = checksum(input.Iterations, input.Seed)
	}
	elapsed := float64(time.Since(began).Nanoseconds()) / 1e6
	return json.NewEncoder(os.Stdout).Encode(struct {
		Checksums []int64 `json:"checksums"`
		ComputeMS float64 `json:"compute_ms"`
	}{checksums, elapsed})
}

type service struct {
	db            *pgxpool.Pool
	pgMode        pgMode
	peer          string
	cpu           chan cpuJob
	waitingTimer  atomic.Int64
	waitingTCP    atomic.Int64
	active        atomic.Int64
	actualCPUJobs atomic.Int64
}

func (s *service) cpuWorker() {
	for job := range s.cpu {
		// A cancelled queued job releases its reservation only when it is
		// actually removed. A running calculation retains it until completion.
		select {
		case <-job.context.Done():
			s.actualCPUJobs.Add(-1)
			continue
		default:
		}
		job.reply <- checksum(job.iterations, job.seed)
		s.actualCPUJobs.Add(-1)
	}
}

func text(w http.ResponseWriter, status int, body string) {
	respond(w, status, body, "text/plain; charset=utf-8")
}

func respond(w http.ResponseWriter, status int, body, contentType string) {
	w.Header().Set("Content-Type", contentType)
	w.Header().Set("Content-Length", strconv.Itoa(len(body)))
	w.WriteHeader(status)
	_, _ = io.WriteString(w, body)
}

func runtimeSnapshot() (map[string]any, []string) {
	// The dedicated metrics request reads these cumulative standard counters.
	// No per-business-request allocation probe or gctrace output is enabled.
	names := []string{
		"/gc/cycles/automatic:gc-cycles", "/gc/cycles/forced:gc-cycles", "/gc/cycles/total:gc-cycles",
		"/gc/heap/allocs:bytes", "/gc/heap/frees:bytes", "/gc/heap/allocs:objects", "/gc/heap/frees:objects",
		"/gc/heap/live:bytes", "/gc/heap/objects:objects",
		"/memory/classes/heap/objects:bytes", "/memory/classes/heap/released:bytes",
		"/cpu/classes/gc/total:cpu-seconds", "/cpu/classes/gc/mark/assist:cpu-seconds",
	}
	samples := make([]metrics.Sample, len(names))
	for index, name := range names {
		samples[index].Name = name
	}
	metrics.Read(samples)
	values := make(map[string]any, len(samples))
	unavailable := []string{}
	for _, sample := range samples {
		switch sample.Value.Kind() {
		case metrics.KindUint64:
			values[sample.Name] = sample.Value.Uint64()
		case metrics.KindFloat64:
			values[sample.Name] = sample.Value.Float64()
		default:
			unavailable = append(unavailable, sample.Name)
		}
	}
	return values, unavailable
}

func (s *service) serve(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path == "/metrics" {
		values, unavailable := runtimeSnapshot()
		body, _ := json.Marshal(map[string]any{"waiting_timer": s.waitingTimer.Load(), "waiting_tcp": s.waitingTCP.Load(), "active": s.active.Load(), "actual_cpu_jobs": s.actualCPUJobs.Load(), "runtime": values, "runtime_unavailable": unavailable})
		text(w, 200, string(body))
		return
	}
	s.active.Add(1)
	defer s.active.Add(-1)
	ctx, cancel := context.WithTimeout(r.Context(), 45*time.Second)
	defer cancel()
	rid := r.URL.Query().Get("rid")
	switch r.URL.Path {
	case "/ok":
		text(w, 200, "ok:"+rid)
	case "/json":
		if r.Method != http.MethodPost {
			text(w, 405, "method_not_allowed")
			return
		}
		body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, 65536))
		var decoded map[string]any
		if err == nil {
			err = json.Unmarshal(body, &decoded)
		}
		counter, ok := decoded["counter"].(float64)
		data, hasData := decoded["data"]
		if err != nil || !ok || !hasData || counter != float64(int64(counter)) || counter < -9007199254740990 || counter > 9007199254740990 {
			text(w, 400, "invalid_json_envelope")
			return
		}
		// json.Unmarshal's native values are serialized afresh. RawMessage and
		// cached serialized data are intentionally not used.
		encoded, err := json.Marshal(map[string]any{"id": rid, "counter": int64(counter) + 1, "data": data})
		if err != nil {
			text(w, 500, "encode_error")
			return
		}
		respond(w, 200, string(encoded), "application/json; charset=utf-8")
	case "/work":
		iterations := int64(len(r.URL.Query().Get("work"))) * 1000
		seed := int64(len(r.URL.Query().Get("seed")))
		if iterations < 1000 || iterations > 1000000 || seed < 1 || seed > 1024 {
			text(w, 400, "invalid_cpu_input")
			return
		}
		job := cpuJob{iterations, seed, make(chan int64, 1), ctx}
		// Reserve before publication so even an immediately scheduled worker
		// cannot finish before this increment. Rejecting a full queue refunds
		// immediately; a metrics read may conservatively see that brief reserve.
		s.actualCPUJobs.Add(1)
		select {
		case s.cpu <- job:
		default:
			s.actualCPUJobs.Add(-1)
			text(w, 503, "cpu_queue_full")
			return
		}
		select {
		case value := <-job.reply:
			text(w, 200, fmt.Sprintf("%s:%08d", rid, value))
		case <-ctx.Done():
			text(w, 504, "deadline")
		}
	case "/timer", "/wait":
		duration, prefix := 25*time.Millisecond, "timer:"
		if r.URL.Path == "/wait" {
			duration, prefix = 35*time.Second, "wait:"
		}
		s.waitingTimer.Add(1)
		timer := time.NewTimer(duration)
		select {
		case <-timer.C:
			s.waitingTimer.Add(-1)
			text(w, 200, prefix+rid)
		case <-ctx.Done():
			timer.Stop()
			s.waitingTimer.Add(-1)
			text(w, 504, "deadline")
		}
	case "/tcp":
		conn, err := (&net.Dialer{Timeout: 3 * time.Second}).DialContext(ctx, "tcp", s.peer)
		if err != nil {
			text(w, 500, "tcp_connect_error")
			return
		}
		defer conn.Close()
		stopCancel := context.AfterFunc(ctx, func() { _ = conn.Close() })
		defer stopCancel()
		_ = conn.SetDeadline(time.Now().Add(40 * time.Second))
		if _, err = conn.Write([]byte("K")); err != nil {
			text(w, 500, "tcp_write_error")
			return
		}
		s.waitingTCP.Add(1)
		var reply [1]byte
		_, err = io.ReadFull(conn, reply[:])
		s.waitingTCP.Add(-1)
		if err != nil || reply[0] != 'K' {
			text(w, 500, "tcp_reply_error")
			return
		}
		text(w, 200, "tcp:"+rid)
	case "/db":
		if s.db == nil {
			text(w, 503, "database_not_configured")
			return
		}
		key, err := strconv.Atoi(r.URL.Query().Get("key"))
		if err != nil || key < 1 || key > 1024 {
			text(w, 400, "invalid_database_key")
			return
		}
		var row pgResult
		row.RID = rid
		// Extended protocol parameters, no statement cache and no query retry.
		if s.pgMode == pgResetEquivalent {
			err = queryResetEquivalent(ctx, s.db, key, &row)
		} else {
			err = s.db.QueryRow(ctx, "SELECT id,label,score FROM cross_stack_items WHERE id=$1", key).Scan(&row.ID, &row.Label, &row.Score)
		}
		if err != nil {
			text(w, 500, "database_query_error")
			return
		}
		body, err := json.Marshal(row)
		if err != nil {
			text(w, 500, "encode_error")
			return
		}
		respond(w, 200, string(body), "application/json; charset=utf-8")
	default:
		text(w, 404, "not_found")
	}
}

func main() {
	// Training-only explicit file; ordinary builds do not profile requests.
	// No HTTP profiling route, implicit default.pgo or production endpoint.
	if name := os.Getenv("BENCH_CPU_PROFILE"); name != "" {
		file, err := os.OpenFile(name, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
		if err != nil {
			log.Fatal("cannot create explicit benchmark CPU profile")
		}
		if err := pprof.StartCPUProfile(file); err != nil {
			_ = file.Close()
			log.Fatal("cannot start explicit benchmark CPU profile")
		}
		defer func() { pprof.StopCPUProfile(); _ = file.Close() }()
	}
	if len(os.Args) == 2 && os.Args[1] == "--batch" {
		if err := batch(); err != nil {
			log.Fatal(err)
		}
		return
	}
	mode, err := parsePGMode(os.Getenv("BENCH_PG_MODE"))
	if err != nil {
		log.Fatal(err)
	}
	workers := envInt("BENCH_WORKERS", 1)
	runtime.GOMAXPROCS(workers)
	s := &service{pgMode: mode, peer: net.JoinHostPort("127.0.0.1", strconv.Itoa(envInt("BENCH_PEER_PORT", 19091))), cpu: make(chan cpuJob, 128)}
	for range envInt("BENCH_CPU_WORKERS", workers) {
		go s.cpuWorker()
	}
	if dbURL := os.Getenv("BENCH_DB_URL"); dbURL != "" {
		cfg, err := pgxpool.ParseConfig(dbURL)
		if err != nil {
			log.Fatal("invalid benchmark database configuration")
		}
		cfg.ConnConfig.DefaultQueryExecMode = pgx.QueryExecModeExec
		cfg.ConnConfig.ConnectTimeout = 2 * time.Second
		cfg.MaxConns = int32(envInt("BENCH_DB_POOL", 16))
		s.db, err = pgxpool.NewWithConfig(context.Background(), cfg)
		if err != nil {
			log.Fatal("cannot create benchmark database pool")
		}
		defer s.db.Close()
	}
	addr := os.Getenv("BENCH_ADDR")
	if addr == "" {
		addr = "127.0.0.1:19090"
	}
	server := &http.Server{Addr: addr, Handler: http.HandlerFunc(s.serve), ReadHeaderTimeout: 3 * time.Second, ReadTimeout: 3 * time.Second, WriteTimeout: 45 * time.Second, IdleTimeout: 5 * time.Second, MaxHeaderBytes: 4096}
	stopping := make(chan os.Signal, 1)
	signal.Notify(stopping, os.Interrupt, syscall.SIGTERM)
	go func() {
		<-stopping
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = server.Shutdown(ctx)
	}()
	log.Printf("Go net/http %s workers=%d cpu_workers=%d GC=%s", addr, workers, envInt("BENCH_CPU_WORKERS", workers), strings.TrimSpace(os.Getenv("GOGC")))
	if err := server.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
		log.Fatal(err)
	}
}
