package main

// This opt-in gate observes a finite HTTP admission boundary with incomplete
// ordinary JSON requests. It is deliberately separate from open-arrival scores.
import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

const admissionActive = 1024
const admissionPending = 128
const admissionConnections = 4096
const admissionExcess = 32
const admissionCancel = 16
const admissionTotal = admissionActive + admissionPending + admissionExcess

type admissionLoop struct {
	Active      int `json:"active_requests"`
	Pending     int `json:"pending"`
	Connections int `json:"connections"`
	Queued      int `json:"queued"`
	Running     int `json:"running"`
	Resident    int `json:"resident"`
}
type admissionMetrics struct {
	Limits struct {
		Active      int `json:"max_active_requests"`
		Pending     int `json:"max_pending_requests"`
		Connections int `json:"max_connections"`
	} `json:"limits"`
	Loops []admissionLoop `json:"loops"`
	Tasks []struct {
		Ready    int `json:"ready"`
		Waiting  int `json:"waiting"`
		Running  int `json:"running"`
		Resident int `json:"resident"`
		Timers   int `json:"timers"`
	} `json:"tasks"`
	TaskDriver struct {
		Resident int `json:"resident"`
	} `json:"task_driver"`
	BufferBytes uint64 `json:"buffer_bytes"`
	Errors      struct {
		Counts map[string]uint64 `json:"counts"`
	} `json:"errors"`
}

func (m admissionMetrics) totals() admissionLoop {
	var total admissionLoop
	for _, v := range m.Loops {
		total.Active += v.Active
		total.Pending += v.Pending
		total.Connections += v.Connections
		total.Queued += v.Queued
		total.Running += v.Running
		total.Resident += v.Resident
	}
	return total
}
func (m admissionMetrics) limitsValid() bool {
	return m.Limits.Active == admissionActive && m.Limits.Pending == admissionPending && m.Limits.Connections == admissionConnections && len(m.Loops) > 0 && len(m.Loops) <= 32
}
func (m admissionMetrics) recovered(baseline admissionMetrics) bool {
	if !m.limitsValid() || m.totals() != (admissionLoop{}) || m.TaskDriver.Resident != 0 || m.BufferBytes > baseline.BufferBytes {
		return false
	}
	for _, task := range m.Tasks {
		if task.Ready != 0 || task.Waiting != 0 || task.Running != 0 || task.Resident != 0 || task.Timers != 0 {
			return false
		}
	}
	return true
}

type admissionRow struct {
	RID          string `json:"rid"`
	Category     string `json:"category"`
	Detail       string `json:"detail,omitempty"`
	ReadCategory string `json:"read_category,omitempty"`
	ReleaseError string `json:"release_error,omitempty"`
	DialStart    int64  `json:"dial_start_ns"`
	Connected    int64  `json:"connected_ns"`
	PartialStart int64  `json:"partial_start_ns"`
	PartialDone  int64  `json:"partial_done_ns"`
	CancelAt     int64  `json:"planned_cancel_ns,omitempty"`
	ReleaseStart int64  `json:"release_start_ns,omitempty"`
	ReleaseDone  int64  `json:"release_done_ns,omitempty"`
	ResponseAt   int64  `json:"response_ns,omitempty"`
	Status       int    `json:"status"`
	BytesWritten int    `json:"bytes_written"`
	BodyBytes    int    `json:"body_bytes"`
}
type admissionResponse struct {
	index, status, bytes int
	at                   time.Time
	category, detail     string
}

func admissionLoopback(address string) error {
	host, port, err := net.SplitHostPort(address)
	if err != nil {
		return fmt.Errorf("admission gate requires numeric loopback host:port")
	}
	ip := net.ParseIP(host)
	n, portErr := strconv.Atoi(port)
	if ip == nil || !ip.IsLoopback() || portErr != nil || n < 1 || n > 65535 {
		return fmt.Errorf("admission gate requires numeric loopback host:port")
	}
	return nil
}
func validateAdmissionConfig(c config) error {
	if !c.admissionPressure {
		if c.admissionAdminAddress != "" {
			return fmt.Errorf("admission-admin-address requires admission-pressure")
		}
		return nil
	}
	if err := admissionLoopback(c.address); err != nil {
		return err
	}
	if err := admissionLoopback(c.admissionAdminAddress); err != nil {
		return err
	}
	if c.address == c.admissionAdminAddress {
		return fmt.Errorf("admission gate requires a separate management ingress")
	}
	if c.events != "" || c.pathOverrideSet || c.responseLiteralSet || c.cpuRate != 0 || c.cpuCancelEvery != 0 {
		return fmt.Errorf("admission gate cannot be combined with arrival/event/path diagnostics")
	}
	if c.workload != "ok" && c.workload != "json_object" {
		return fmt.Errorf("admission gate only performs the frozen JSON object workload")
	}
	token := os.Getenv("KU_SERVICE_CONTROL_TOKEN")
	if len(token) < 1 || len(token) > 128 || strings.ContainsAny(token, "\r\n") {
		return fmt.Errorf("admission gate requires a 1..128 byte control token in KU_SERVICE_CONTROL_TOKEN")
	}
	for _, b := range []byte(token) {
		if b < 0x21 || b > 0x7e {
			return fmt.Errorf("admission gate control token must contain printable non-space ASCII")
		}
	}
	if admissionTotal > 8192 || admissionTotal >= admissionConnections || admissionCancel >= admissionActive {
		return fmt.Errorf("invalid bounded admission plan")
	}
	if len(c.corpora["object"].bytes) != 1048 || c.protocol.Endpoints["json_object"].Method != "POST" || c.protocol.Endpoints["json_object"].Path != "/json" {
		return fmt.Errorf("admission gate requires the frozen 1048-byte JSON object POST contract")
	}
	return nil
}

func admissionWrite(conn net.Conn, p []byte) (int, error) {
	total := 0
	for len(p) > 0 {
		n, err := conn.Write(p)
		total += n
		p = p[n:]
		if err != nil {
			return total, err
		}
		if n == 0 {
			return total, io.ErrNoProgress
		}
	}
	return total, nil
}
func admissionRead(conn net.Conn, method string) (int, []byte, error) {
	response, err := readResponse(bufio.NewReaderSize(conn, 4096), &http.Request{Method: method})
	if err != nil {
		return 0, nil, fmt.Errorf("response header: %w", err)
	}
	defer response.Body.Close()
	body, err := io.ReadAll(io.LimitReader(response.Body, maxBody+1))
	if len(body) > maxBody {
		return response.StatusCode, nil, fmt.Errorf("response body exceeds 64KiB")
	}
	if err != nil {
		return response.StatusCode, body, fmt.Errorf("response body: %w", err)
	}
	return response.StatusCode, body, err
}

func parseAdmissionMetrics(body []byte) (admissionMetrics, error) {
	var metrics admissionMetrics
	if err := json.Unmarshal(body, &metrics); err != nil {
		return metrics, fmt.Errorf("management JSON: %w", err)
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(body, &fields); err != nil {
		return metrics, fmt.Errorf("management JSON object required")
	}
	for _, key := range []string{"limits", "loops", "tasks", "task_driver", "buffer_bytes", "outcomes", "errors", "transport_closes"} {
		if len(fields[key]) == 0 || string(fields[key]) == "null" {
			return metrics, fmt.Errorf("management metrics missing %s", key)
		}
	}
	if !metrics.limitsValid() {
		return metrics, fmt.Errorf("actual management limits do not equal C=4096 A=1024 P=128")
	}
	if len(metrics.Tasks) != len(metrics.Loops) || metrics.Errors.Counts == nil {
		return metrics, fmt.Errorf("management task/error accounting incomplete")
	}
	for _, loop := range metrics.Loops {
		if loop.Active < 0 || loop.Pending < 0 || loop.Connections < 0 || loop.Queued < 0 || loop.Running < 0 || loop.Resident < 0 {
			return metrics, fmt.Errorf("negative management occupancy")
		}
	}
	return metrics, nil
}
func admissionState(address, token string) (admissionMetrics, json.RawMessage, error) {
	var metrics admissionMetrics
	deadline := time.Now().Add(300 * time.Millisecond)
	conn, err := net.DialTimeout("tcp", address, 300*time.Millisecond)
	if err != nil {
		return metrics, nil, fmt.Errorf("management dial: %w", err)
	}
	defer conn.Close()
	if err = conn.SetDeadline(deadline); err != nil {
		return metrics, nil, err
	}
	request := "GET /__ku/metrics HTTP/1.1\r\nHost: " + address + "\r\nX-Ku-Control-Token: " + token + "\r\nConnection: close\r\n\r\n"
	if _, err = admissionWrite(conn, []byte(request)); err != nil {
		return metrics, nil, fmt.Errorf("management write: %w", err)
	}
	status, body, err := admissionRead(conn, "GET")
	if err != nil {
		return metrics, nil, fmt.Errorf("management read: %w", err)
	}
	if status != 200 {
		return metrics, nil, fmt.Errorf("management status %d", status)
	}
	metrics, err = parseAdmissionMetrics(body)
	return metrics, json.RawMessage(body), err
}

func admissionRequest(c config, rid string) []byte {
	body := c.corpora["object"].bytes
	return append([]byte(fmt.Sprintf("POST /json?rid=%s HTTP/1.1\r\nHost: %s\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n", rid, c.address, len(body))), body[:len(body)-1]...)
}
func admissionResponseClass(c config, rid string, status int, body []byte, err error) (string, string) {
	if err != nil {
		category := "protocol_error"
		var transport *net.OpError
		var timed net.Error
		switch {
		case errors.As(err, &timed) && timed.Timeout():
			category = "transport_timeout"
		case errors.Is(err, syscall.ECONNRESET):
			category = "transport_reset"
		case errors.Is(err, net.ErrClosed):
			category = "transport_closed"
		case errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF):
			category = "transport_eof"
		case errors.As(err, &transport):
			category = "transport_error"
		}
		return category, truncate(err.Error())
	}
	if status == 503 {
		return "explicit_503", ""
	}
	if status != 200 {
		return "unexpected_status", fmt.Sprintf("HTTP %d", status)
	}
	if len(body) != 1071 {
		return "body_error", fmt.Sprintf("response bytes %d, expected1071", len(body))
	}
	if err = validate(c, job{kind: "json_object", rid: rid}, body); err != nil {
		return "body_error", truncate(err.Error())
	}
	return "correct_200", ""
}

func runAdmissionPressure(c config) (out map[string]any, err error) {
	if err = validateAdmissionConfig(c); err != nil {
		return nil, err
	}
	origin, anchor := captureMonotonicAnchor(time.Now, readKernelMonotonic)
	rows := make([]admissionRow, admissionTotal)
	connections := make([]net.Conn, admissionTotal)
	requests := make([][]byte, admissionTotal)
	done := make([]bool, admissionTotal)
	planned := make([]bool, admissionTotal)
	responses := make(chan admissionResponse, admissionTotal)
	var readers sync.WaitGroup
	snapshots := make([]map[string]any, 0, 8)
	observations := make([]map[string]any, 0, 128)
	process := make([]map[string]any, 0, 8)
	out = map[string]any{
		"schema": "ku-admission-pressure-v1", "passed": false, "started_utc": origin.UTC().Format(time.RFC3339Nano),
		"formal_throughput_score": false, "scope": "finite JSON admission boundary; not maximum throughput or CPU/Rust ranking",
		"address": c.address, "management_address": c.admissionAdminAddress, "protocol_sha256": c.protocolHash,
		"corpus":          map[string]any{"name": "object", "sha256": c.corpora["object"].hash, "input_bytes": 1048, "response_bytes": 1071, "rid_prefix": c.prefix, "rid_format": "9-digit prefix +7-digit request index; 16-digit string"},
		"expected_limits": map[string]int{"max_connections": admissionConnections, "max_active_requests": admissionActive, "max_pending_requests": admissionPending},
		"plan":            map[string]any{"connections": admissionTotal, "excess": admissionExcess, "planned_cancel": admissionCancel, "body_bytes": 1048, "response_bytes": 1071, "dial_parallelism": 64, "dial_phase_ms": 1500, "partial_send_phase_ms": 500, "occupancy_phase_ms": 600, "response_deadline_from_partial_send_ms": 2500, "recovery_limit_ms": 5000, "recovery_requests": 8, "replay": false},
		"runtime":         map[string]any{"go": runtime.Version(), "goos": runtime.GOOS, "goarch": runtime.GOARCH, "cpus": runtime.NumCPU(), "gomaxprocs": runtime.GOMAXPROCS(0)},
	}
	clocks := map[string]any{"definition": "all *_ns request fields are Go monotonic offsets from origin; driver startup is not subtracted"}
	anchor.annotate(clocks)
	out["clocks"] = clocks
	var pressureReleased time.Time
	var baseline admissionMetrics
	baselineIdle := false
	var failure string
	markProcess := func(phase string) {
		sample := admissionProcessSnapshot()
		sample["phase"] = phase
		sample["at_ns"] = time.Since(origin).Nanoseconds()
		process = append(process, sample)
	}
	state := func(phase string, retain bool) (admissionMetrics, error) {
		m, raw, e := admissionState(c.admissionAdminAddress, os.Getenv("KU_SERVICE_CONTROL_TOKEN"))
		observation := map[string]any{"phase": phase, "at_ns": time.Since(origin).Nanoseconds(), "totals": m.totals(), "buffer_bytes": m.BufferBytes}
		if e != nil {
			observation["error"] = e.Error()
		}
		if len(observations) < 128 {
			observations = append(observations, observation)
		}
		if retain && len(snapshots) < 12 {
			snapshots = append(snapshots, map[string]any{"phase": phase, "at_ns": time.Since(origin).Nanoseconds(), "metrics": raw})
		}
		return m, e
	}
	consume := func(response admissionResponse) {
		i := response.index
		if done[i] {
			failure = "duplicate response accounting"
			return
		}
		done[i] = true
		rows[i].Category, rows[i].Detail = response.category, response.detail
		rows[i].ReadCategory = response.category
		rows[i].Status, rows[i].BodyBytes, rows[i].ResponseAt = response.status, response.bytes, response.at.Sub(origin).Nanoseconds()
		if planned[i] {
			if rows[i].ResponseAt < rows[i].CancelAt {
				failure = "request terminated before its planned cancellation"
			} else {
				rows[i].Category = "planned_cancel"
				if response.status != 0 {
					failure = "incomplete planned cancellation received an unexpected HTTP response"
				}
			}
		}
	}
	drain := func() {
		for {
			select {
			case response := <-responses:
				consume(response)
			default:
				return
			}
		}
	}
	// Always retire only this driver's own descriptors, wait for bounded readers,
	// then observe recovery, including after setup or protocol failure.
	defer func() {
		closeStarted := time.Now()
		withdraw := pressureReleased
		if withdraw.IsZero() {
			withdraw = closeStarted
			out["recovery_deadline_origin"] = "owned socket withdrawal after setup failure"
		} else {
			out["recovery_deadline_origin"] = "before releasing held request bodies; includes response completion and resource retirement"
		}
		for _, conn := range connections {
			if conn != nil {
				conn.Close()
			}
		}
		out["driver_sockets_closed_ns"] = time.Since(origin).Nanoseconds()
		readers.Wait()
		drain()
		markProcess("withdrawn")
		recovered := false
		recoveryDeadline := withdraw.Add(5 * time.Second)
		if baselineIdle {
			for time.Now().Before(recoveryDeadline) {
				m, e := state("recovery", false)
				if e == nil && m.recovered(baseline) {
					recovered = true
					break
				}
				time.Sleep(50 * time.Millisecond)
			}
		}
		out["resources_recovered_before_probe"] = recovered
		finalResources := false
		if recovered {
			_, _ = state("recovered_before_probe", true)
			probe := make([]map[string]any, 0, 8)
			for i := 0; i < 8 && time.Now().Before(recoveryDeadline); i++ {
				rid := fmt.Sprintf("%09d%07d", c.prefix, admissionTotal+i)
				start := time.Now()
				deadline := minTime(recoveryDeadline, start.Add(300*time.Millisecond))
				conn, e := net.DialTimeout("tcp", c.address, time.Until(deadline))
				status := 0
				var body []byte
				if e == nil {
					e = conn.SetDeadline(deadline)
					if e == nil {
						p := admissionRequest(c, rid)
						p = append(p, c.corpora["object"].bytes[1047])
						_, e = admissionWrite(conn, p)
					}
					if e == nil {
						status, body, e = admissionRead(conn, "POST")
					}
					conn.Close()
				}
				category, detail := admissionResponseClass(c, rid, status, body, e)
				probe = append(probe, map[string]any{"rid": rid, "category": category, "detail": detail, "status": status, "body_bytes": len(body), "duration_ns": time.Since(start).Nanoseconds(), "finished_ns": time.Since(origin).Nanoseconds()})
				if category != "correct_200" {
					recovered = false
					break
				}
			}
			if len(probe) != 8 {
				recovered = false
			}
			out["recovery_probe"] = probe
			if recovered {
				recovered = false
				for time.Now().Before(recoveryDeadline) {
					m, e := state("after_recovery_probe", false)
					if e == nil && m.recovered(baseline) {
						recovered = true
						break
					}
					time.Sleep(10 * time.Millisecond)
				}
			}
			m, e := state("final", true)
			finalResources = e == nil && m.recovered(baseline) && time.Now().Before(recoveryDeadline)
		}
		out["resources_recovered"] = finalResources
		recovered = recovered && finalResources && time.Now().Before(recoveryDeadline)
		out["normal_flow_recovered_within_5s"] = recovered
		out["withdrawn_ns"], out["recovery_elapsed_ns"] = withdraw.Sub(origin).Nanoseconds(), time.Since(withdraw).Nanoseconds()
		if !recovered && failure == "" {
			failure = "resources or normal flow did not recover within5s"
		}
		counts := map[string]int{}
		for i := range rows {
			if rows[i].Category == "" {
				rows[i].Category = "not_completed"
			}
			counts[rows[i].Category]++
		}
		if failure == "" && (counts["explicit_503"] != admissionExcess || counts["planned_cancel"] != admissionCancel || counts["correct_200"] != admissionActive+admissionPending-admissionCancel) {
			failure = "terminal outcomes differ from32 explicit503,16 planned_cancel,1136 correct200"
		}
		markProcess("finished")
		out["passed"], out["failure"] = failure == "", failure
		out["counts"], out["requests"], out["management_snapshots"], out["management_observations"], out["driver_process"] = counts, rows, snapshots, observations, process
		out["elapsed_ns"] = time.Since(origin).Nanoseconds()
	}()
	markProcess("start")
	preparation, e := prepareDescriptorTable(admissionTotal + 2)
	out["descriptor_preparation"] = map[string]any{"supported": preparation.supported, "method": preparation.method, "soft_limit": preparation.softLimit, "max_open_fd": preparation.maxOpenFD, "target_fd": preparation.targetFD, "limit_clamped": preparation.limitClamped, "duplicate_closed": preparation.duplicateClosed, "source_closed": preparation.sourceClosed, "elapsed_ns": preparation.finished.Sub(preparation.started).Nanoseconds()}
	if e != nil || preparation.supported && preparation.limitClamped {
		failure = "insufficient or failed bounded descriptor preparation"
		if e != nil {
			failure += "; " + e.Error()
		}
		return out, nil
	}
	baseline, e = state("baseline", true)
	if e != nil {
		failure = e.Error()
		return out, nil
	}
	if !baseline.recovered(baseline) {
		failure = "baseline is not exclusive idle service state"
		return out, nil
	}
	baselineIdle = true
	indices := make(chan int, admissionTotal)
	out["request_prepare_start_ns"] = time.Since(origin).Nanoseconds()
	for i := range rows {
		rows[i].RID = fmt.Sprintf("%09d%07d", c.prefix, i)
		requests[i] = admissionRequest(c, rows[i].RID)
		indices <- i
	}
	out["requests_prepared_ns"] = time.Since(origin).Nanoseconds()
	close(indices)
	ctx, cancel := context.WithTimeout(context.Background(), 1500*time.Millisecond)
	var dialers sync.WaitGroup
	for worker := 0; worker < 64; worker++ {
		dialers.Add(1)
		go func() {
			defer dialers.Done()
			for i := range indices {
				rows[i].DialStart = time.Since(origin).Nanoseconds()
				conn, e := (&net.Dialer{}).DialContext(ctx, "tcp", c.address)
				if e != nil {
					rows[i].Category = "dial_error"
					rows[i].Detail = truncate(e.Error())
				} else {
					connections[i] = conn
					rows[i].Connected = time.Since(origin).Nanoseconds()
				}
			}
		}()
	}
	dialers.Wait()
	cancel()
	markProcess("connected")
	out["connections_ready_ns"] = time.Since(origin).Nanoseconds()
	for _, conn := range connections {
		if conn == nil {
			failure = "not all finite connections established within1.5s"
			return out, nil
		}
	}
	sendOrigin := time.Now()
	out["partial_send_origin_ns"] = sendOrigin.Sub(origin).Nanoseconds()
	responseDeadline := sendOrigin.Add(2500 * time.Millisecond)
	var senders sync.WaitGroup
	for i, conn := range connections {
		senders.Add(1)
		go func(i int, conn net.Conn) {
			defer senders.Done()
			rows[i].PartialStart = time.Since(origin).Nanoseconds()
			e := conn.SetDeadline(responseDeadline)
			n := 0
			if e == nil {
				n, e = admissionWrite(conn, requests[i])
			}
			rows[i].BytesWritten = n
			rows[i].PartialDone = time.Since(origin).Nanoseconds()
			if e != nil {
				rows[i].Category = "partial_write_error"
				rows[i].Detail = truncate(e.Error())
				return
			}
			readers.Add(1)
			go func() {
				defer readers.Done()
				status, body, e := admissionRead(conn, "POST")
				category, detail := admissionResponseClass(c, rows[i].RID, status, body, e)
				responses <- admissionResponse{i, status, len(body), time.Now(), category, detail}
			}()
		}(i, conn)
	}
	senders.Wait()
	markProcess("partial_sent")
	out["partial_send_complete_ns"] = time.Since(origin).Nanoseconds()
	for _, row := range rows {
		if row.Category != "" {
			failure = "partial request write failed"
			return out, nil
		}
	}
	if time.Since(sendOrigin) > 500*time.Millisecond {
		failure = "driver partial-send phase exceeded500ms; boundary sample invalid"
		return out, nil
	}
	occupied := false
	occupancyDeadline := time.Now().Add(600 * time.Millisecond)
	for time.Now().Before(occupancyDeadline) {
		drain()
		rejected := 0
		for i, row := range rows {
			if !done[i] {
				continue
			}
			if row.Category != "explicit_503" {
				failure = "unexpected response before releasing incomplete body"
				return out, nil
			}
			rejected++
		}
		m, e := state("filling", false)
		if e != nil {
			failure = e.Error()
			return out, nil
		}
		if total := m.totals(); total.Active == admissionActive && total.Pending == admissionPending && rejected == admissionExcess && m.Errors.Counts["queue_admission"] >= baseline.Errors.Counts["queue_admission"] && m.Errors.Counts["queue_admission"]-baseline.Errors.Counts["queue_admission"] == admissionExcess {
			occupied = true
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if !occupied {
		failure = "did not observe actual A=1024 P=128 and32 explicit503 within600ms"
		return out, nil
	}
	_, e = state("occupied", true)
	if e != nil {
		failure = e.Error()
		return out, nil
	}
	markProcess("occupied")
	cancelled := 0
	for i, conn := range connections {
		if !done[i] && cancelled < admissionCancel {
			planned[i] = true
			rows[i].CancelAt = time.Since(origin).Nanoseconds()
			conn.Close()
			cancelled++
		}
	}
	_, e = state("after_planned_cancel", true)
	if e != nil {
		failure = e.Error()
		return out, nil
	}
	pressureReleased = time.Now()
	out["release_origin_ns"] = pressureReleased.Sub(origin).Nanoseconds()
	for i, conn := range connections {
		if done[i] || planned[i] {
			continue
		}
		senders.Add(1)
		go func(i int, conn net.Conn) {
			defer senders.Done()
			rows[i].ReleaseStart = time.Since(origin).Nanoseconds()
			n, e := admissionWrite(conn, c.corpora["object"].bytes[1047:])
			rows[i].BytesWritten += n
			rows[i].ReleaseDone = time.Since(origin).Nanoseconds()
			if e != nil {
				rows[i].ReleaseError = truncate(e.Error())
			}
		}(i, conn)
	}
	senders.Wait()
	markProcess("released")
	out["release_complete_ns"] = time.Since(origin).Nanoseconds()
	for _, row := range rows {
		if row.ReleaseError != "" {
			failure = "one or more final body byte writes failed"
			break
		}
	}
	readers.Wait()
	drain()
	_, e = state("responses_finished", true)
	if e != nil {
		failure = e.Error()
	}
	return out, nil
}

func minTime(a, b time.Time) time.Time {
	if a.Before(b) {
		return a
	}
	return b
}
