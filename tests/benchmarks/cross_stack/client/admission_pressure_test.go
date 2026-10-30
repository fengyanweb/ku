package main

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"
)

func TestAdmissionLoopbackOnly(t *testing.T) {
	for _, address := range []string{"127.0.0.1:8080", "127.2.3.4:80", "[::1]:8080"} {
		if err := admissionLoopback(address); err != nil {
			t.Fatalf("%s: %v", address, err)
		}
	}
	for _, address := range []string{"localhost:80", "0.0.0.0:80", "192.168.1.1:80", "example.com:80", "127.0.0.1:http", "127.0.0.1:0", "127.0.0.1:65536", "127.0.0.1:80\r\n"} {
		if err := admissionLoopback(address); err == nil {
			t.Fatalf("accepted %q", address)
		}
	}
}

func TestAdmissionIncompleteRequestAndExactJSONOracle(t *testing.T) {
	c := testConfig(t)
	c.address = "127.0.0.1:8080"
	rid := "1234567890000001"
	request := admissionRequest(c, rid)
	parts := strings.SplitN(string(request), "\r\n\r\n", 2)
	if len(parts) != 2 || !strings.Contains(parts[0], "Content-Length: 1048\r\n") || len(parts[1]) != 1047 || parts[1] != string(c.corpora["object"].bytes[:1047]) || !strings.Contains(parts[0], "POST /json?rid="+rid+" HTTP/1.1") {
		t.Fatal("not exactly one withheld byte from the frozen ordinary JSON request")
	}
	input := c.corpora["object"]
	response, err := json.Marshal(map[string]any{"id": rid, "counter": input.counter + 1, "data": input.data})
	if err != nil {
		t.Fatal(err)
	}
	if len(response) != 1071 {
		t.Fatalf("frozen response length=%d", len(response))
	}
	if category, _ := admissionResponseClass(c, rid, 200, response, nil); category != "correct_200" {
		t.Fatal(category)
	}
	if category, _ := admissionResponseClass(c, "1234567890000002", 200, response, nil); category != "body_error" {
		t.Fatal(category)
	}
	if category, _ := admissionResponseClass(c, rid, 0, nil, fmt.Errorf("connection reset by peer")); category == "explicit_503" {
		t.Fatal("unknown reset classified as admission rejection")
	}
	if category, _ := admissionResponseClass(c, rid, 503, []byte("unavailable"), nil); category != "explicit_503" {
		t.Fatal(category)
	}
}

func TestAdmissionActualLimitsAndResourceRecovery(t *testing.T) {
	m := admissionMetrics{}
	m.Limits.Active = admissionActive
	m.Limits.Pending = admissionPending
	m.Limits.Connections = admissionConnections
	m.Loops = []admissionLoop{{}}
	if !m.limitsValid() || !m.recovered(m) {
		t.Fatal("valid idle metrics rejected")
	}
	base := m
	m.Loops = []admissionLoop{{Active: admissionActive, Pending: admissionPending, Connections: admissionTotal}}
	if m.recovered(base) {
		t.Fatal("occupied resources accepted as recovered")
	}
	m.Loops = []admissionLoop{{}}
	m.BufferBytes = 1
	if m.recovered(base) {
		t.Fatal("retained app buffer accepted as recovered")
	}
	m.BufferBytes = 0
	m.TaskDriver.Resident = 1
	if m.recovered(base) {
		t.Fatal("retained task accepted as recovered")
	}
	m.TaskDriver.Resident = 0
	m.Limits.Active--
	if m.limitsValid() {
		t.Fatal("lowered service limit accepted")
	}
}

func TestAdmissionMetricsRequireRealLimitsAndAccounting(t *testing.T) {
	valid := []byte(`{"limits":{"max_active_requests":1024,"max_pending_requests":128,"max_connections":4096},"loops":[{"active_requests":0,"pending":0,"connections":0,"queued":0,"running":0,"resident":0}],"tasks":[{"ready":0,"waiting":0,"running":0,"resident":0,"timers":0}],"task_driver":{"resident":0},"buffer_bytes":0,"outcomes":[],"errors":{"counts":{"queue_admission":0}},"transport_closes":[]}`)
	metrics, err := parseAdmissionMetrics(valid)
	if err != nil || !metrics.recovered(metrics) {
		t.Fatalf("valid metrics: %v", err)
	}
	for _, bad := range [][]byte{[]byte(`{}`), []byte(strings.Replace(string(valid), `"max_active_requests":1024`, `"max_active_requests":16`, 1)), []byte(strings.Replace(string(valid), `"tasks":[{"ready":0,"waiting":0,"running":0,"resident":0,"timers":0}]`, `"tasks":[]`, 1)), []byte(strings.Replace(string(valid), `"buffer_bytes":0,`, "", 1))} {
		if _, err := parseAdmissionMetrics(bad); err == nil {
			t.Fatalf("accepted incomplete/lowered accounting %s", bad)
		}
	}
}

func TestAdmissionFlagsAreOptInAndTokenNotCopiedIntoConfig(t *testing.T) {
	c := testConfig(t)
	if err := validateAdmissionConfig(c); err != nil {
		t.Fatal(err)
	}
	c.admissionAdminAddress = "127.0.0.1:8081"
	if err := validateAdmissionConfig(c); err == nil {
		t.Fatal("management flag accepted without opt-in")
	}
	c.admissionPressure = true
	c.address = "127.0.0.1:8080"
	t.Setenv("KU_SERVICE_CONTROL_TOKEN", "unit-only-not-an-actual-token")
	if err := validateAdmissionConfig(c); err != nil {
		t.Fatal(err)
	}
	c.address = "192.168.1.10:8080"
	if err := validateAdmissionConfig(c); err == nil {
		t.Fatal("nonloopback pressure accepted")
	}
	c.address = "127.0.0.1:8080"
	t.Setenv("KU_SERVICE_CONTROL_TOKEN", "invalid\r\nHeader: injected")
	if err := validateAdmissionConfig(c); err == nil {
		t.Fatal("control-token header injection accepted")
	}
}
