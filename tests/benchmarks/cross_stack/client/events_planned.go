package main

import (
	"fmt"
	"math"
	"strconv"
	"strings"
	"time"
	"unsafe"
)

// Only this audit backing is limited here. It is not the whole Go heap budget;
// the unchanged driver cgroup still limits all runtime, transport and CSV memory.
const maxPlannedEventBytes uint64 = 256 * 1024 * 1024
const maxEventKindBytes = 64
const maxEventRIDBytes = 16
const maxPlannedCSVRowBytes uint64 = 512
const maxPlannedCSVHeaderBytes uint64 = 512

// Pointer-free rows never retain request bodies, error details, time.Location,
// request objects or per-plan RID strings. The original CSV has no detail column.
// unsafe.Sizeof is checked before allocation rather than assumed by the budget.
type plannedEventSlot struct {
	scheduled, sent, completed, began, generated, cancelDue, cancelIssued int64
	streamIndex                                                           uint64
	bytesWritten, bytesRead, dialNS, status                               int64
	flags, kindID, terminalID, stageID                                    uint16
	rid                                                                   [maxEventRIDBytes]byte
	ridLen                                                                uint8
}

var eventTerminalNames = []string{"", "body_error", "cancel_issue_error", "connection_error", "correct", "not_started_deadline", "not_started_queue", "not_started_scheduler_deadline", "other_http", "planned_reset", "protocol_error", "rejected_429", "rejected_503", "timeout"}
var eventStageNames = []string{"", "TCP_linger0_close", "before_dial_deadline", "before_write_deadline", "body_limit", "completed_after_absolute_deadline", "connection_pool_closed", "deadline", "dial", "planned_reset_after_absolute_deadline", "read_body", "read_parse_header", "reset_close", "reset_due_after_absolute_deadline", "reset_linger", "reset_requires_TCP", "unexpected_content_encoding", "validate", "write"}

const (
	eventScheduledPresent uint16 = 1 << iota
	eventSentPresent
	eventCompletedPresent
	eventBeganPresent
	eventGeneratedPresent
	eventCancelDuePresent
	eventCancelIssuedPresent
	eventLate
	eventReused
	eventWriteComplete
	eventCPUCancel
)

type plannedEventStore struct {
	slots                                 []plannedEventSlot
	seen                                  []uint64
	kinds                                 []string
	kindCodes, terminalCodes, stageCodes  map[string]uint16
	origin                                time.Time
	hasOrigin                             bool
	stored, rejected, duplicates, missing uint64
	backingBytes                          uint64
	err                                   string
}

func eventNameCodes(names []string) map[string]uint16 {
	result := make(map[string]uint16, len(names))
	for i, name := range names {
		result[name] = uint16(i)
	}
	return result
}

func plannedEventCount(c config) (uint64, error) {
	short := math.Ceil(c.duration.Seconds() * c.rate)
	long := float64(0)
	if c.workload == "task_mix" {
		long = math.Ceil(c.duration.Seconds() * c.cpuRate)
	}
	if math.IsNaN(short) || math.IsInf(short, 0) || math.IsNaN(long) || math.IsInf(long, 0) || short < 1 || long < 0 || short+long > 10000000 {
		return 0, fmt.Errorf("invalid finite event plan count")
	}
	return uint64(short) + uint64(long), nil
}

func plannedEventAllocation(count uint64) (uint64, uint64, error) {
	slotBytes := uint64(unsafe.Sizeof(plannedEventSlot{}))
	if slotBytes != 128 {
		return 0, 0, fmt.Errorf("unexpected event slot size %d, expected128", slotBytes)
	}
	if count == 0 || count > 10000000 || count > uint64(int(^uint(0)>>1)) || count > math.MaxUint64/slotBytes {
		return 0, 0, fmt.Errorf("event plan exceeds checked count bound")
	}
	words := (count + 63) / 64
	rowBytes := count * slotBytes
	if words > math.MaxUint64/8 || rowBytes > math.MaxUint64-words*8 {
		return 0, 0, fmt.Errorf("event backing byte count overflows")
	}
	total := rowBytes + words*8
	if total > maxPlannedEventBytes {
		return 0, 0, fmt.Errorf("event backing needs%d bytes, limit%d before arrival origin", total, maxPlannedEventBytes)
	}
	// These are upper bounds for every emitted field, not observed average rows.
	// Count<=10m gives index/streamIndex<=7 digits; RID<=16 bytes, kind<=64,
	// 7 signed offsets and4 signed numeric values<=20 chars each,4 bools<=5,
	// terminal<=30/stage<=37 ASCII,21 separators. CSV escaping is included:
	// worst arbitrary RID/kind quote doubling still yields<=506 bytes/row.
	if count > (uint64(maxEvents)-maxPlannedCSVHeaderBytes)/maxPlannedCSVRowBytes {
		return 0, 0, fmt.Errorf("planned CSV could exceed existing uncompressed byte limit")
	}
	return words, total, nil
}

func newPlannedEventStore(c config, count uint64) (*plannedEventStore, error) {
	if err := checkPlannedEventLabels(); err != nil {
		return nil, err
	}
	words, total, err := plannedEventAllocation(count)
	if err != nil {
		return nil, err
	}
	kinds := []string{c.workload}
	if c.workload == "task_mix" {
		kinds = []string{"ok", "cpu"}
	}
	if c.workload == "mixed" {
		kinds = []string{"json_object", "db", "timer", "cpu"}
	}
	for _, kind := range kinds {
		if len(kind) > maxEventKindBytes {
			return nil, fmt.Errorf("event kind exceeds64byte field bound")
		}
	}
	p := &plannedEventStore{slots: make([]plannedEventSlot, int(count)), seen: make([]uint64, int(words)), kinds: kinds, backingBytes: total}
	p.kindCodes, p.terminalCodes, p.stageCodes = eventNameCodes(kinds), eventNameCodes(eventTerminalNames), eventNameCodes(eventStageNames)
	return p, nil
}

func openPlannedEvents(path string, c config, count uint64) (*eventWriter, error) {
	p, err := newPlannedEventStore(c, count)
	if err != nil {
		return nil, err
	}
	// Reuse the original exclusive file, 21/15-column formatter and compressor.
	// Its writer waits for EOF export; no CSV events enter its channel in-flight.
	w, err := openEvents(path, c.workload == "task_mix" || c.diagnosticArrival)
	if err != nil {
		return nil, err
	}
	w.planned = p
	return w, nil
}

func (p *plannedEventStore) reject(reason string) {
	p.rejected++
	if p.err == "" {
		p.err = reason
	}
}

// Called only from finish while holding the unchanged ledger mutex.
func (p *plannedEventStore) offer(e event) {
	index := e.j.index
	if index >= uint64(len(p.slots)) {
		p.reject("event index outside preplanned backing")
		return
	}
	word, bit := index/64, uint64(1)<<(index%64)
	if p.seen[word]&bit != 0 {
		p.duplicates++
		p.reject("duplicate terminal event index")
		return
	}
	if len(e.j.rid) > maxEventRIDBytes || e.j.streamIndex >= uint64(len(p.slots)) {
		p.reject("event RID or stream index exceeds preplanned bound")
		return
	}
	kind, kindOK := p.kindCodes[e.j.kind]
	terminal, terminalOK := p.terminalCodes[e.result.category]
	stage, stageOK := p.stageCodes[e.result.stage]
	if !kindOK || !terminalOK || !stageOK {
		p.reject("event label is outside bounded known labels")
		return
	}
	if !p.hasOrigin {
		p.origin, p.hasOrigin = e.origin, true
	} else if !p.origin.Equal(e.origin) {
		p.reject("event origin differs inside one plan")
		return
	}
	row := plannedEventSlot{streamIndex: e.j.streamIndex, bytesWritten: e.result.bytesWritten, bytesRead: e.result.bytesRead, dialNS: e.result.dialTime.Nanoseconds(), status: int64(e.result.status), kindID: kind, terminalID: terminal, stageID: stage, ridLen: uint8(len(e.j.rid))}
	copy(row.rid[:], e.j.rid)
	times := [7]time.Time{e.j.scheduled, e.result.sent, e.result.completed, e.began, e.j.generated, e.result.cancelDue, e.result.cancelIssued}
	offsets := [7]*int64{&row.scheduled, &row.sent, &row.completed, &row.began, &row.generated, &row.cancelDue, &row.cancelIssued}
	for i, timestamp := range times {
		if !timestamp.IsZero() {
			row.flags |= uint16(1) << i
			*offsets[i] = timestamp.Sub(e.origin).Nanoseconds()
		}
	}
	if e.result.late {
		row.flags |= eventLate
	}
	if e.result.reused {
		row.flags |= eventReused
	}
	if e.result.writeComplete {
		row.flags |= eventWriteComplete
	}
	if e.j.cpuCancel {
		row.flags |= eventCPUCancel
	}
	p.slots[index] = row
	p.seen[word] |= bit
	p.stored++
}

func (p *plannedEventStore) event(index int) event {
	row := p.slots[index]
	at := func(offset int64, present uint16) time.Time {
		if row.flags&present == 0 {
			return time.Time{}
		}
		return p.origin.Add(time.Duration(offset))
	}
	return event{origin: p.origin, began: at(row.began, eventBeganPresent),
		j:      job{index: uint64(index), streamIndex: row.streamIndex, cpuCancel: row.flags&eventCPUCancel != 0, kind: p.kinds[row.kindID], rid: string(row.rid[:row.ridLen]), scheduled: at(row.scheduled, eventScheduledPresent), generated: at(row.generated, eventGeneratedPresent)},
		result: result{category: eventTerminalNames[row.terminalID], stage: eventStageNames[row.stageID], reused: row.flags&eventReused != 0, dialTime: time.Duration(row.dialNS), status: int(row.status), sent: at(row.sent, eventSentPresent), completed: at(row.completed, eventCompletedPresent), bytesWritten: row.bytesWritten, bytesRead: row.bytesRead, late: row.flags&eventLate != 0, writeComplete: row.flags&eventWriteComplete != 0, cancelDue: at(row.cancelDue, eventCancelDuePresent), cancelIssued: at(row.cancelIssued, eventCancelIssuedPresent)}}
}

// Requests/arrival generation are over before this deliberate blocking export.
// CPU/memory stay in the same driver cgroup; a separate lifecycle gate must
// include this tail. completed/run_end request clocks are deliberately unchanged.
func (w *eventWriter) exportPlanned() {
	p := w.planned
	w.exportStarted = time.Now()
	p.missing = uint64(len(p.slots)) - p.stored
	for index := range p.slots {
		if p.seen[index/64]&(uint64(1)<<(uint(index)%64)) == 0 {
			continue
		}
		w.queue <- p.event(index)
	}
	close(w.queue)
	<-w.done
	w.exportFinished = time.Now()
	w.dropped = p.missing
	if p.err != "" {
		if w.err != "" {
			w.err += "; "
		}
		w.err += p.err
	}
	if p.missing != 0 && p.err == "" {
		if w.err != "" {
			w.err += "; "
		}
		w.err += "missing preplanned terminal events"
	}
}

func (w *eventWriter) plannedReport(origin time.Time) map[string]any {
	p := w.planned
	if p == nil {
		return nil
	}
	return map[string]any{"mode": "preplanned-compact-slots", "capacity_rows": len(p.slots), "stored_rows": p.stored, "rejected_offers": p.rejected, "duplicate_offers": p.duplicates, "missing_rows": p.missing, "slot_bytes": unsafe.Sizeof(plannedEventSlot{}), "backing_bytes": p.backingBytes, "backing_limit_bytes": maxPlannedEventBytes, "field_dictionaries": "bounded kind<=64bytes; known static terminal/stage; no detail/body retention", "CSV_order": "plan index; timestamps unchanged", "export_started_offset_ns": w.exportStarted.Sub(origin).Nanoseconds(), "export_finished_offset_ns": w.exportFinished.Sub(origin).Nanoseconds(), "export_elapsed_ns": w.exportFinished.Sub(w.exportStarted).Nanoseconds(), "lifecycle_CPU_gate_required": true, "scope": "backing+bitmap requested bytes; excludes Go heap/transport/compressor, all remain in unchanged driver cgroup"}
}

// Keep the upper-bound proof easy to review when adding any new label.
func checkPlannedEventLabels() error {
	for _, name := range eventTerminalNames {
		if len(name) > 30 || strings.ContainsAny(name, "\r\n,\"") {
			return fmt.Errorf("event terminal exceeds CSV proof")
		}
	}
	for _, name := range eventStageNames {
		if len(name) > 37 || strings.ContainsAny(name, "\r\n,\"") {
			return fmt.Errorf("event stage exceeds CSV proof")
		}
	}
	if len(strconv.FormatUint(9999999, 10)) != 7 {
		return fmt.Errorf("event index proof changed")
	}
	return nil
}
