package main

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"strings"
	"testing"
	"time"
)

func TestCodecLines(t *testing.T) {
	base := `{"v":1,"type":"future","padding":""}`
	for _, size := range []int{maxLineBytes - 1, maxLineBytes, maxLineBytes + 1} {
		t.Run(fmt.Sprint(size), func(t *testing.T) {
			line := strings.Replace(base, `"padding":""`, `"padding":"`+strings.Repeat("x", size-len(base)-1)+`"`, 1) + "\n"
			if len(line) != size {
				t.Fatal(len(line))
			}
			_, err := newCodec(strings.NewReader(line)).read()
			if (err != nil) != (size > maxLineBytes) {
				t.Fatalf("size %d: %v", size, err)
			}
		})
	}
	for _, line := range []string{
		`[]` + "\n", `null` + "\n", `{"v":2,"type":"state"}` + "\n",
		`{"v":1,"type":"state","generation":0,"remotes":[]}`, // No terminating newline.
		"{\"v\":1,\"type\":\"\xff\"}\n",
		`{"v":1,"type":"state","generation":-1,"remotes":[]}` + "\n",
		`{"v":1,"type":"state","generation":18446744073709551616,"remotes":[]}` + "\n",
		`{"v":1,"type":"sign_ok"}` + "\n",
		`{"v":1,"type":"sign_ok","id":-1}` + "\n",
		`{"v":1,"type":"sign_ok","id":"1"}` + "\n",
		`{"v":1,"type":"shutdown","deadline_ms":25001}` + "\n",
	} {
		if _, err := newCodec(strings.NewReader(line)).read(); err == nil {
			t.Errorf("accepted malformed frame %q", line)
		}
	}
	if _, err := newCodec(strings.NewReader("")).read(); err != io.EOF {
		t.Fatal(err)
	}
}

func TestCodecUnknownAndIDs(t *testing.T) {
	codec := newCodec(strings.NewReader("{\"v\":1,\"type\":\"future\",\"id\":\"not-our-schema\"}\n" +
		"{\"v\":1,\"type\":\"state\",\"generation\":18446744073709551615,\"remotes\":[],\"id\":\"unknown-member\",\"future\":true}\n"))
	m, err := codec.read()
	if err != nil || m.Type != "future" {
		t.Fatalf("unknown type: %+v %v", m, err)
	}
	m, err = codec.read()
	if err != nil || *m.Generation != ^uint64(0) {
		t.Fatalf("u64 generation and unknown member: %+v %v", m, err)
	}
	for _, kind := range []string{"sign_ok", "sign_failed", "tls_sign_ok", "tls_sign_failed"} {
		m, err = newCodec(strings.NewReader(fmt.Sprintf("{\"v\":1,\"type\":%q,\"id\":18446744073709551615}\n", kind))).read()
		if err != nil || *m.ID != ^uint64(0) {
			t.Fatalf("%s id: %+v %v", kind, m, err)
		}
	}
	var b bytes.Buffer
	if err := (&output{w: &b}).send("ready", map[string]any{"version": version}); err != nil {
		t.Fatal(err)
	}
	var envelope map[string]any
	if err := json.Unmarshal(b.Bytes(), &envelope); err != nil || envelope["v"] != float64(1) || envelope["type"] != "ready" {
		t.Fatalf("output envelope: %s %v", b.Bytes(), err)
	}
}

func TestSignerCorrelatesIDs(t *testing.T) {
	r, w := io.Pipe()
	s := newSigner(&output{w: w}, func(string, error) {})
	defer close(s.done)
	defer r.Close()
	defer w.Close()
	for expected := uint64(1); expected <= 2; expected++ {
		result := make(chan error, 1)
		go func() {
			jwt, sig, err := s.sign("hub", []byte("abcdefghijklmno"))
			if err == nil && (jwt != "leaf-jwt" || len(sig) != 64) {
				err = fmt.Errorf("wrong sign result %q %x", jwt, sig)
			}
			result <- err
		}()
		var request struct {
			V        int    `json:"v"`
			Type     string `json:"type"`
			ID       uint64 `json:"id"`
			Remote   string `json:"remote"`
			NonceB64 string `json:"nonce_b64"`
		}
		if err := json.NewDecoder(r).Decode(&request); err != nil {
			t.Fatal(err)
		}
		if request.V != 1 || request.Type != "sign" || request.ID != expected || request.Remote != "hub" || request.NonceB64 != "abcdefghijklmno" {
			t.Fatalf("bad sign request: %+v", request)
		}
		for _, unknown := range []uint64{0, 99} {
			s.reply(message{Type: "sign_ok", ID: &unknown})
		}
		// An unrelated TLS response must not consume a pending leaf request.
		s.reply(message{Type: "tls_sign_ok", ID: &request.ID})
		select {
		case err := <-result:
			t.Fatalf("unmatched response completed request: %v", err)
		default:
		}
		s.reply(message{Type: "sign_ok", ID: &request.ID, JWT: "leaf-jwt", SignatureB64: base64.RawURLEncoding.EncodeToString(make([]byte, 64))})
		s.reply(message{Type: "sign_ok", ID: &request.ID}) // Duplicate is ignored.
		select {
		case err := <-result:
			if err != nil {
				t.Fatal(err)
			}
		case <-time.After(time.Second):
			t.Fatal("signer did not complete")
		}
	}
}

func TestSignerFailureAndTimeout(t *testing.T) {
	for _, reason := range []string{"vault unavailable", "timed out"} {
		t.Run(reason, func(t *testing.T) {
			s := newSigner(&output{w: io.Discard}, func(string, error) {})
			defer close(s.done)
			if reason != "timed out" {
				go func() {
					for {
						s.mu.Lock()
						_, pending := s.pending[1]
						s.mu.Unlock()
						if pending {
							id := uint64(1)
							s.reply(message{Type: "sign_failed", ID: &id, Signer: "ck-bus", Reason: reason})
							return
						}
						time.Sleep(time.Millisecond)
					}
				}()
			}
			_, _, err := s.sign("hub", nil)
			if err == nil || err.Error() != "leaf signer via ck-bus: "+reason {
				t.Fatalf("wrong signer error: %v", err)
			}
		})
	}
}

func TestSignerConcurrentRequestOrder(t *testing.T) {
	r, w := io.Pipe()
	s := newSigner(&output{w: w}, func(string, error) {})
	defer close(s.done)
	defer r.Close()
	defer w.Close()
	results := make(chan error, 16)
	for i := range 16 {
		go func() {
			_, _, err := s.sign(fmt.Sprint(i), []byte("abcdefghijklmno"))
			results <- err
		}()
	}
	decoder := json.NewDecoder(r)
	for expected := uint64(1); expected <= 16; expected++ {
		var request struct {
			ID uint64 `json:"id"`
		}
		if err := decoder.Decode(&request); err != nil {
			t.Fatal(err)
		}
		if request.ID != expected {
			t.Fatalf("wire id %d, want %d", request.ID, expected)
		}
		s.reply(message{Type: "sign_ok", ID: &request.ID, JWT: "jwt", SignatureB64: base64.RawURLEncoding.EncodeToString(make([]byte, 64))})
	}
	for range 16 {
		if err := <-results; err != nil {
			t.Fatal(err)
		}
	}
}
