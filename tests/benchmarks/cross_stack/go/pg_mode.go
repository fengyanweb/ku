package main

import (
	"context"
	"fmt"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
)

type pgMode uint8

const (
	pgNormal pgMode = iota
	pgResetEquivalent
)

func parsePGMode(value string) (pgMode, error) {
	switch value {
	case "", "normal":
		return pgNormal, nil
	case "reset-equivalent":
		return pgResetEquivalent, nil
	default:
		return pgNormal, fmt.Errorf("invalid BENCH_PG_MODE: expected normal or reset-equivalent")
	}
}

type pgResult struct {
	ID    int    `json:"id"`
	Label string `json:"label"`
	Score int    `json:"score"`
	RID   string `json:"rid"`
}

// This is an explicitly selected comparison contract, never the normal path.
// Both acquisition and reset consume the same absolute operation deadline.
func queryResetEquivalent(parent context.Context, pool *pgxpool.Pool, key int, row *pgResult) error {
	operation, cancel := context.WithTimeout(parent, 2*time.Second)
	defer cancel()
	acquire, stopAcquire := context.WithTimeout(operation, 250*time.Millisecond)
	lease, err := pool.Acquire(acquire)
	stopAcquire()
	if err != nil {
		return err
	}
	return queryResetLease(operation, lease, key, row)
}

// QueryRow.Scan has materialized the scalar fields before reset. A reset
// failure therefore evicts the session but does not discard the owned result
// or retry the query. Hijack prevents any Fast/normal recycle of a dirty lease.
func queryResetLease(operation context.Context, lease *pgxpool.Conn, key int, row *pgResult) error {
	reusable := false
	defer func() {
		if reusable {
			lease.Release()
		} else {
			_ = lease.Hijack().Close(operation)
		}
	}()
	if err := lease.QueryRow(operation, "SELECT id,label,score FROM cross_stack_items WHERE id=$1", key).Scan(&row.ID, &row.Label, &row.Score); err != nil {
		return err
	}
	// Exec completes only after the protocol response has been drained. It is
	// deliberately invoked on this lease, not on the pool or a second session.
	_, err := lease.Exec(operation, "DISCARD ALL")
	reusable = err == nil
	return nil
}
