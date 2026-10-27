package main

import (
	"compress/gzip"
	"encoding/csv"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"
	"unsafe"
)

func readPlannedCSV(t *testing.T, path string) [][]string {
	t.Helper()
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
	if err != nil {
		t.Fatal(err)
	}
	return rows
}

func TestPlannedEventsCheckedBackingAndFrozen900s(t *testing.T) {
	c := config{workload: "task_mix", duration: 900 * time.Second, rate: 2000, cpuRate: 151.85}
	count, err := plannedEventCount(c)
	if err != nil || count != 1936665 {
		t.Fatal(count, err)
	}
	_, total, err := plannedEventAllocation(count)
	if err != nil || total > maxPlannedEventBytes || unsafe.Sizeof(plannedEventSlot{}) != 128 {
		t.Fatal(total, err)
	}
	// Validate the exact byte calculation without allocating the900s backing.
	if total != count*128+((count+63)/64)*8 {
		t.Fatal(total)
	}
	for _, bad := range []uint64{0, math.MaxUint64, 10000001, 2100000} {
		if _, _, err := plannedEventAllocation(bad); err == nil {
			t.Fatalf("accepted unrepresentable/budget count%d", bad)
		}
	}
	// This is a no-pointer type, not a slice of retained event/result strings.
	typ := reflect.TypeOf(plannedEventSlot{})
	for i := 0; i < typ.NumField(); i++ {
		kind := typ.Field(i).Type.Kind()
		if kind == reflect.String || kind == reflect.Pointer || kind == reflect.Slice || kind == reflect.Map || kind == reflect.Interface {
			t.Fatal(typ.Field(i))
		}
	}
}

func TestPlannedEventsHaveNoInFlightCSVOverflowBeyondOldQueue(t *testing.T) {
	const count = 9001
	path := filepath.Join(t.TempDir(), "no-overflow.csv.gz")
	c := config{workload: "task_mix"}
	writer, err := openPlannedEvents(path, c, count)
	if err != nil {
		t.Fatal(err)
	}
	origin := time.Now()
	for i := 0; i < count; i++ {
		writer.offer(event{origin: origin, j: job{index: uint64(i), streamIndex: uint64(i), kind: "ok", rid: fmt.Sprintf("%016d", i), scheduled: origin, generated: origin}, result: result{category: "correct", completed: origin.Add(time.Millisecond)}})
	}
	// The exporter has deliberately received no request rows for the entire
	// offered burst. This is a fully stalled in-flight exporter, not a race
	// against favorable disk speed; the old8192record queue could not hold it.
	if writer.planned.stored != count || writer.written != 0 || len(writer.queue) != 0 {
		t.Fatal(writer.planned.stored, writer.written, len(writer.queue))
	}
	writer.close()
	if writer.written != count || writer.dropped != 0 || writer.err != "" {
		t.Fatal(writer.written, writer.dropped, writer.err)
	}
	rows := readPlannedCSV(t, path)
	if len(rows) != count+1 || len(rows[0]) != 21 {
		t.Fatal(len(rows))
	}
	for i, row := range rows[1:] {
		if row[0] != fmt.Sprintf("%016d", i) || row[2] != fmt.Sprint(i) {
			t.Fatal(i, row)
		}
	}
}

func TestPlannedEventsPreserveAll21FieldsAndNullVersusZero(t *testing.T) {
	path := filepath.Join(t.TempDir(), "exact.csv.gz")
	writer, err := openPlannedEvents(path, config{workload: "task_mix"}, 2)
	if err != nil {
		t.Fatal(err)
	}
	origin := time.Now()
	second := event{origin: origin, began: origin.Add(-time.Microsecond), j: job{index: 1, streamIndex: 1, cpuCancel: true, kind: "cpu", rid: "reset", scheduled: origin, generated: origin.Add(100 * time.Microsecond)}, result: result{category: "planned_reset", stage: "TCP_linger0_close", status: 200, bytesWritten: 220, bytesRead: 19, sent: origin, completed: origin.Add(time.Millisecond), reused: true, late: true, dialTime: 2 * time.Microsecond, writeComplete: true, cancelDue: origin.Add(250 * time.Microsecond), cancelIssued: origin.Add(300 * time.Microsecond)}}
	writer.offer(second)
	writer.offer(event{origin: origin, j: job{index: 0, kind: "ok", rid: "unsent", scheduled: origin}, result: result{category: "not_started_queue", completed: origin.Add(time.Millisecond)}})
	writer.close()
	if writer.err != "" {
		t.Fatal(writer.err)
	}
	rows := readPlannedCSV(t, path)
	want := []string{"reset", "cpu", "1", "0", "0", "1000000", "-1000", "planned_reset", "200", "220", "19", "true", "true", "2000", "TCP_linger0_close", "1", "true", "250000", "300000", "true", "100000"}
	if len(rows) != 3 || !reflect.DeepEqual(rows[2], want) {
		t.Fatal(rows)
	}
	if rows[1][4] != "" || rows[1][6] != "" || rows[1][20] != "" || rows[1][3] != "0" {
		t.Fatal("null/zero timestamps changed", rows[1])
	}
}

func TestPlannedEventsRejectDuplicatesOutOfRangeAndUnknownWithoutOverwrite(t *testing.T) {
	path := filepath.Join(t.TempDir(), "invalid.csv.gz")
	writer, err := openPlannedEvents(path, config{workload: "task_mix"}, 2)
	if err != nil {
		t.Fatal(err)
	}
	origin := time.Now()
	row := event{origin: origin, j: job{index: 0, kind: "ok", rid: "original", scheduled: origin}, result: result{category: "correct", completed: origin.Add(time.Millisecond)}}
	writer.offer(row)
	row.j.rid = "duplicate"
	writer.offer(row)
	row.j.index = 2
	writer.offer(row)
	row.j.index = 1
	row.result.stage = "unregistered"
	writer.offer(row)
	writer.close()
	if writer.planned.stored != 1 || writer.planned.duplicates != 1 || writer.planned.rejected != 3 || writer.planned.missing != 1 || writer.dropped != 1 || writer.err == "" {
		t.Fatal(writer.planned, writer.dropped, writer.err)
	}
	rows := readPlannedCSV(t, path)
	if len(rows) != 2 || rows[1][0] != "original" {
		t.Fatal(rows)
	}
}

func TestPlannedEventsRejectLongLabelBeforeOpeningOutput(t *testing.T) {
	path := filepath.Join(t.TempDir(), "not-created.csv.gz")
	if _, err := openPlannedEvents(path, config{workload: strings.Repeat("k", 65)}, 1); err == nil {
		t.Fatal("accepted unsupported field budget")
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatal("startup rejection created output", err)
	}
	if _, err := openPlannedEvents(path, config{workload: "ok"}, 2100000); err == nil {
		t.Fatal("accepted audit backing over256MiB")
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatal("budget rejection created output", err)
	}
}
