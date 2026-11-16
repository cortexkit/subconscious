package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestEngineChild(t *testing.T) {
	if os.Getenv("CK_NATS_ENGINE_TEST_CHILD") != "1" {
		t.Skip("subprocess entry point")
	}
	os.Exit(run(os.Getenv("CK_NATS_ENGINE_TEST_CONFIG"), os.Stdin, os.Stdout))
}

type engineChild struct {
	input  io.WriteCloser
	frames chan map[string]any
	exit   chan error
	stderr *bytes.Buffer
}

func startEngine(t *testing.T) engineChild {
	t.Helper()
	dir := t.TempDir()
	config := filepath.Join(dir, "server.conf")
	text := fmt.Sprintf("host: 127.0.0.1\nport: -1\nhttp_port: -1\njetstream {store_dir: %q, max_memory_store: 16MB, max_file_store: 32MB}\n", filepath.ToSlash(filepath.Join(dir, "js")))
	if err := os.WriteFile(config, []byte(text), 0600); err != nil {
		t.Fatal(err)
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(executable, "-test.run=^TestEngineChild$")
	cmd.Env = append(os.Environ(), "CK_NATS_ENGINE_TEST_CHILD=1", "CK_NATS_ENGINE_TEST_CONFIG="+config)
	input, err := cmd.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	stderr := new(bytes.Buffer)
	cmd.Stderr = stderr
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	frames := make(chan map[string]any, 100)
	go func() {
		defer close(frames)
		scanner := bufio.NewScanner(stdout)
		scanner.Buffer(make([]byte, maxLineBytes), maxLineBytes)
		for scanner.Scan() {
			var m map[string]any
			if json.Unmarshal(scanner.Bytes(), &m) != nil {
				frames <- map[string]any{"invalid_line": scanner.Text()}
				continue
			}
			frames <- m
		}
	}()
	exit := make(chan error, 1)
	go func() { exit <- cmd.Wait() }()
	t.Cleanup(func() {
		_ = input.Close()
		_ = cmd.Process.Kill()
	})
	return engineChild{input, frames, exit, stderr}
}

func (c engineChild) next(t *testing.T, kind string) map[string]any {
	t.Helper()
	deadline := time.After(7 * time.Second)
	for {
		select {
		case m, ok := <-c.frames:
			if !ok {
				t.Fatalf("engine stdout ended before %s", kind)
			}
			if m["v"] != float64(1) {
				t.Fatalf("stdout is not a protocol frame: %+v", m)
			}
			if m["type"] == kind {
				return m
			}
		case <-deadline:
			t.Fatalf("no %s frame", kind)
		}
	}
}

func (c engineChild) wait(t *testing.T, code int) {
	t.Helper()
	select {
	case err := <-c.exit:
		actual := 0
		if err != nil {
			if e, ok := err.(*exec.ExitError); ok {
				actual = e.ExitCode()
			} else {
				t.Fatal(err)
			}
		}
		if actual != code {
			t.Fatalf("exit %d, want %d; stderr: %s", actual, code, c.stderr.String())
		}
	case <-time.After(5 * time.Second):
		t.Fatal("engine did not exit within 5 seconds")
	}
}

func TestEngineLifecycle(t *testing.T) {
	c := startEngine(t)
	ready := c.next(t, "ready")
	if ready["version"] != "0.1.0" || ready["nats_version"] != "2.15.3" || ready["patch"] != "reload-signaturecb-1" {
		t.Fatalf("unexpected ready: %+v", ready)
	}
	status := c.next(t, "status")
	js, ok := status["jetstream"].(map[string]any)
	if !ok || js["enabled"] != true || js["ok"] != true || len(status["leafs"].([]any)) != 0 || status["listener"] != nil {
		t.Fatalf("unexpected local status: %+v", status)
	}
	_, _ = io.WriteString(c.input, "{\"v\":1,\"type\":\"future\"}\n{\"v\":1,\"type\":\"future\"}\n")
	for _, generation := range []int{0, 1, 0} {
		_, err := fmt.Fprintf(c.input, "{\"v\":1,\"type\":\"state\",\"generation\":%d,\"remotes\":[]}\n", generation)
		if err != nil {
			t.Fatal(err)
		}
		if got := c.next(t, "applied")["generation"]; got != float64(generation) {
			t.Fatalf("generation %v, want %d", got, generation)
		}
	}
	// Even without changes, the wrapper receives fresh health at least every five seconds.
	c.next(t, "status")
	if err := c.input.Close(); err != nil {
		t.Fatal(err)
	}
	c.wait(t, 0)
	if !strings.Contains(c.stderr.String(), "JetStream") {
		t.Fatal("server logs did not go to stderr")
	}
	if strings.Count(c.stderr.String(), `ignored unknown message type "future"`) != 1 {
		t.Fatal("unknown types were not logged exactly once")
	}
}

func TestEngineProtocolExitAndShutdown(t *testing.T) {
	for _, tc := range []struct {
		name  string
		input string
		code  int
	}{
		{"shutdown", "{\"v\":1,\"type\":\"shutdown\",\"deadline_ms\":5000}\n", 0},
		{"wrong version", "{\"v\":2,\"type\":\"shutdown\"}\n", 2},
		{"not object", "[]\n", 2},
		{"unterminated", "{}", 2},
		{"oversized", strings.Repeat(" ", maxLineBytes) + "\n", 2},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := startEngine(t)
			c.next(t, "ready")
			if _, err := io.WriteString(c.input, tc.input); err != nil {
				t.Fatal(err)
			}
			_ = c.input.Close()
			c.wait(t, tc.code)
		})
	}
}

func TestEngineFailedStateRetainsGeneration(t *testing.T) {
	c := startEngine(t)
	c.next(t, "ready")
	_, _ = io.WriteString(c.input, "{\"v\":1,\"type\":\"state\",\"generation\":0,\"remotes\":[]}\n")
	c.next(t, "applied")
	_, _ = io.WriteString(c.input, "{\"v\":1,\"type\":\"state\",\"generation\":1,\"remotes\":[],\"hub_listener\":{\"port\":7422,\"cert_der_b64\":\"AA\"}}\n")
	failed := c.next(t, "apply_failed")
	if failed["generation"] != float64(1) || failed["error"] != "hub listener is not implemented" {
		t.Fatalf("unexpected apply_failed: %+v", failed)
	}
	_, _ = io.WriteString(c.input, "{\"v\":1,\"type\":\"state\",\"generation\":1,\"remotes\":[]}\n")
	if c.next(t, "applied")["generation"] != float64(1) {
		t.Fatal("failed generation was incorrectly committed")
	}
	_ = c.input.Close()
	c.wait(t, 0)
}
