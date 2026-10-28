package main

import (
	"context"
	"os"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

func TestPGModeRequiresExplicitSelection(t *testing.T) {
	for _, value := range []string{"", "normal"} {
		mode, err := parsePGMode(value)
		if mode != pgNormal || err != nil {
			t.Fatalf("normal mode %q: %v %v", value, mode, err)
		}
	}
	if mode, err := parsePGMode("reset-equivalent"); mode != pgResetEquivalent || err != nil {
		t.Fatalf("explicit mode: %v %v", mode, err)
	}
	for _, value := range []string{"reset", "RESET-EQUIVALENT", " normal "} {
		if _, err := parsePGMode(value); err == nil {
			t.Fatalf("accepted unknown mode %q", value)
		}
	}
}

// The explicit private-fixture URL is required; these tests never discover or
// connect to a developer's default DB. No permanent table or user row is changed.
func privatePG(t *testing.T, tracer pgx.QueryTracer) *pgxpool.Pool {
	t.Helper()
	url := os.Getenv("BENCH_PG_TEST_URL")
	if url == "" {
		t.Skip("BENCH_PG_TEST_URL not set; controlled fixture acceptance pending")
	}
	cfg, err := pgxpool.ParseConfig(url)
	if err != nil {
		t.Fatal("invalid private fixture URL")
	}
	cfg.MaxConns = 1
	cfg.ConnConfig.DefaultQueryExecMode = pgx.QueryExecModeExec
	cfg.ConnConfig.Tracer = tracer
	pool, err := pgxpool.NewWithConfig(context.Background(), cfg)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(pool.Close)
	return pool
}

func TestResetEquivalentSameLeaseClearsSession(t *testing.T) {
	pool := privatePG(t, nil)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	lease, err := pool.Acquire(ctx)
	if err != nil {
		t.Fatal(err)
	}
	pid := lease.Conn().PgConn().PID()
	if _, err = lease.Exec(ctx, "SET application_name='ku_reset_probe'; CREATE TEMP TABLE ku_reset_probe(value int)"); err != nil {
		lease.Release()
		t.Fatal(err)
	}
	lease.Release()
	var row pgResult
	if err := queryResetEquivalent(ctx, pool, 17, &row); err != nil || row.ID != 17 || row.Label == "" {
		t.Fatalf("materialized result: id=%d error=%v", row.ID, err)
	}
	lease, err = pool.Acquire(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer lease.Release()
	if got := lease.Conn().PgConn().PID(); got != pid {
		t.Fatalf("successful reset replaced session: %d -> %d", pid, got)
	}
	var name string
	var missing bool
	if err := lease.QueryRow(ctx, "SELECT current_setting('application_name'),to_regclass('pg_temp.ku_reset_probe') IS NULL").Scan(&name, &missing); err != nil || name == "ku_reset_probe" || !missing {
		t.Fatalf("session not reset: name=%q temp_missing=%v err=%v", name, missing, err)
	}
}

type cancelAfterMaterialize struct {
	cancel context.CancelFunc
	count  atomic.Int32
}

type queryMarker struct{}

func (c *cancelAfterMaterialize) TraceQueryStart(ctx context.Context, _ *pgx.Conn, data pgx.TraceQueryStartData) context.Context {
	return context.WithValue(ctx, queryMarker{}, strings.HasPrefix(data.SQL, "SELECT id,label,score FROM cross_stack_items"))
}
func (c *cancelAfterMaterialize) TraceQueryEnd(ctx context.Context, _ *pgx.Conn, data pgx.TraceQueryEndData) {
	if selected, _ := ctx.Value(queryMarker{}).(bool); selected && data.Err == nil {
		c.count.Add(1)
		c.cancel()
	}
}

func TestResetFailureEvictsWithoutLosingOwnedResultOrRetry(t *testing.T) {
	operation, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	tracer := &cancelAfterMaterialize{cancel: cancel}
	pool := privatePG(t, tracer)
	lease, err := pool.Acquire(operation)
	if err != nil {
		t.Fatal(err)
	}
	var row pgResult
	if err := queryResetLease(operation, lease, 17, &row); err != nil || row.ID != 17 || row.Label == "" {
		t.Fatalf("reset failure changed detached result: id=%d err=%v", row.ID, err)
	}
	if tracer.count.Load() != 1 || pool.Stat().TotalConns() != 0 {
		t.Fatalf("query replayed or failed lease retained: queries=%d connections=%d", tracer.count.Load(), pool.Stat().TotalConns())
	}
}
