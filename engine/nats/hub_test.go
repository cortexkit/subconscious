package main

import (
	"bufio"
	"bytes"
	"crypto"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/tls"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func freeHubPort(t *testing.T) int {
	t.Helper()
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := l.Addr().(*net.TCPAddr).Port
	_ = l.Close()
	return port
}

func hubState(t *testing.T, c engineChild, generation int, port int, der []byte) {
	t.Helper()
	err := json.NewEncoder(c.input).Encode(map[string]any{"v": 1, "type": "state", "generation": generation, "remotes": []remote{}, "hub_listener": hubListener{uint16(port), base64.RawURLEncoding.EncodeToString(der)}})
	if err != nil {
		t.Fatal(err)
	}
}

func dialHubTLS(t *testing.T, port int, maxVersion uint16) *tls.Conn {
	t.Helper()
	conn, err := net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", port), time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if err := conn.SetDeadline(time.Now().Add(7 * time.Second)); err != nil {
		t.Fatal(err)
	}
	// A NATS leaf listener sends its plaintext INFO before switching to TLS.
	line, err := bufio.NewReader(conn).ReadString('\n')
	if err != nil || !strings.HasPrefix(line, "INFO ") {
		t.Fatalf("leaf INFO: %q %v", line, err)
	}
	return tls.Client(conn, &tls.Config{InsecureSkipVerify: true, MinVersion: maxVersion, MaxVersion: maxVersion})
}

func handshakeHub(t *testing.T, c engineChild, port int, der []byte, key ed25519.PrivateKey) {
	t.Helper()
	conn := dialHubTLS(t, port, tls.VersionTLS13)
	defer conn.Close()
	result := make(chan error, 1)
	go func() { result <- conn.Handshake() }()
	request := c.next(t, "tls_sign")
	message, err := base64.RawURLEncoding.Strict().DecodeString(request["message_b64"].(string))
	prefix := append(bytes.Repeat([]byte(" "), 64), []byte("TLS 1.3, server CertificateVerify\x00")...)
	if err != nil || request["scheme"] != "ed25519" || !bytes.HasPrefix(message, prefix) || (len(message) != len(prefix)+32 && len(message) != len(prefix)+48) {
		t.Fatalf("not a whole Ed25519 TLS CertificateVerify: %+v", request)
	}
	if err := json.NewEncoder(c.input).Encode(map[string]any{"v": 1, "type": "tls_sign_ok", "id": request["id"], "signature_b64": base64.RawURLEncoding.EncodeToString(ed25519.Sign(key, message))}); err != nil {
		t.Fatal(err)
	}
	if err := <-result; err != nil {
		t.Fatal(err)
	}
	state := conn.ConnectionState()
	if state.Version != tls.VersionTLS13 || !bytes.Equal(state.PeerCertificates[0].Raw, der) {
		t.Fatalf("unexpected handshake: %+v", state)
	}
}

func TestHubStartsAfterListenerAndRefusesChanges(t *testing.T) {
	clientPort, leafPort := freeHubPort(t), freeHubPort(t)
	c := startEngineWith(t, true, clientPort)
	_, _ = io.WriteString(c.input, "{\"v\":1,\"type\":\"state\",\"generation\":0,\"remotes\":[]}\n")
	c.next(t, "applied")
	if conn, err := net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", clientPort), time.Second); err == nil {
		conn.Close()
		t.Fatal("hub started its client listener before receiving hub_listener")
	}
	_, cert, key := certificate(t, nil, nil, false)
	hubState(t, c, 1, leafPort, cert.Raw)
	c.next(t, "ready")
	if c.next(t, "applied")["generation"] != float64(1) {
		t.Fatal("initial listener not applied")
	}
	status := c.next(t, "status")
	listener := status["listener"].(map[string]any)
	if listener["port"] != float64(leafPort) || listener["bound"] != true {
		t.Fatalf("listener status: %+v", status)
	}
	handshakeHub(t, c, leafPort, cert.Raw, key)
	oldTLS := dialHubTLS(t, leafPort, tls.VersionTLS12)
	if err := oldTLS.Handshake(); err == nil {
		t.Fatal("hub accepted TLS 1.2")
	}
	_ = oldTLS.Close()
	_, changedCert, _ := certificate(t, nil, nil, false)
	for _, change := range []struct {
		port int
		der  []byte
	}{{leafPort, changedCert.Raw}, {freeHubPort(t), cert.Raw}} {
		hubState(t, c, 2, change.port, change.der)
		failed := c.next(t, "apply_failed")
		if failed["generation"] != float64(2) || !strings.Contains(failed["error"].(string), "restart required") {
			t.Fatalf("listener change accepted: %+v", failed)
		}
	}
	// A refused generation does not prevent the unchanged listener from reloading.
	hubState(t, c, 2, leafPort, cert.Raw)
	c.next(t, "applied")
	handshakeHub(t, c, leafPort, cert.Raw, key)
	_ = c.input.Close()
	c.wait(t, 0)
}

func TestHubStopsWhileWaitingForListener(t *testing.T) {
	for _, frame := range []string{"", "{\"v\":1,\"type\":\"shutdown\",\"deadline_ms\":1000}\n", "{\"v\":2,\"type\":\"state\"}\n"} {
		c := startEngineWith(t, true, -1)
		_, _ = io.WriteString(c.input, frame)
		_ = c.input.Close()
		code := 0
		if strings.Contains(frame, "\"v\":2") {
			code = 2
		}
		c.wait(t, code)
	}
}

func TestHubWaitsForConfigUntilListener(t *testing.T) {
	config := filepath.Join(t.TempDir(), "server.conf")
	c := launchEngine(t, true, config)
	for _, generation := range []int{0, 1} {
		_, _ = fmt.Fprintf(c.input, "{\"v\":1,\"type\":\"state\",\"generation\":%d,\"remotes\":[]}\n", generation)
		if c.next(t, "applied")["generation"] != float64(generation) {
			t.Fatal("idle state not applied without config")
		}
	}
	_, cert, _ := certificate(t, nil, nil, false)
	port := freeHubPort(t)
	hubState(t, c, 2, port, cert.Raw)
	failed := c.next(t, "apply_failed")
	if failed["generation"] != float64(2) || !strings.Contains(failed["error"].(string), "server.conf") {
		t.Fatalf("missing configuration not reported: %+v", failed)
	}
	if err := os.WriteFile(config, []byte("not_a_nats_option: true\n"), 0600); err != nil {
		t.Fatal(err)
	}
	hubState(t, c, 3, port, cert.Raw)
	if c.next(t, "apply_failed")["generation"] != float64(3) {
		t.Fatal("invalid configuration not refused")
	}
	select {
	case err := <-c.exit:
		t.Fatalf("idle engine exited: %v", err)
	default:
	}
	if err := os.WriteFile(config, []byte("host: 127.0.0.1\nport: -1\nleafnodes {host: 127.0.0.1, no_advertise: true}\n"), 0600); err != nil {
		t.Fatal(err)
	}
	hubState(t, c, 4, port, cert.Raw)
	c.next(t, "ready")
	if c.next(t, "applied")["generation"] != float64(4) {
		t.Fatal("listener did not start after config arrived")
	}
	if c.next(t, "status")["listener"].(map[string]any)["bound"] != true {
		t.Fatal("listener not bound")
	}
	_ = c.input.Close()
	c.wait(t, 0)
}

func TestHubSignerRepliesAndBounds(t *testing.T) {
	for _, test := range []struct{ name, reply, reason string }{
		{"success", "tls_sign_ok", ""},
		{"failure", "tls_sign_failed", "vault unavailable"},
		{"invalid", "tls_sign_ok", "invalid signature reply"},
		{"timeout", "", "timed out"},
		{"eof", "", "wrapper gone"},
	} {
		t.Run(test.name, func(t *testing.T) {
			r, w := io.Pipe()
			defer r.Close()
			defer w.Close()
			channel := newSigner(&output{w: w}, func(string, error) {})
			defer close(channel.done)
			public, key, err := ed25519.GenerateKey(rand.Reader)
			if err != nil {
				t.Fatal(err)
			}
			signer := &hubSigner{public, channel}
			if !bytes.Equal(signer.Public().(ed25519.PublicKey), public) {
				t.Fatal("wrong public key")
			}
			message := []byte("the whole message, not a digest")
			result := make(chan error, 1)
			started := time.Now()
			go func() {
				sig, err := signer.Sign(rand.Reader, message, crypto.Hash(0))
				if err == nil && !ed25519.Verify(public, message, sig) {
					err = fmt.Errorf("wrong signature")
				}
				result <- err
			}()
			var request struct {
				ID                       uint64
				Type, MessageB64, Scheme string
			}
			var frame map[string]any
			if err := json.NewDecoder(r).Decode(&frame); err != nil {
				t.Fatal(err)
			}
			request.ID = uint64(frame["id"].(float64))
			if frame["type"] != "tls_sign" || frame["scheme"] != "ed25519" || frame["message_b64"] != base64.RawURLEncoding.EncodeToString(message) {
				t.Fatalf("bad request: %+v", frame)
			}
			channel.reply(messageReply("sign_ok", request.ID, ""))
			select {
			case err := <-result:
				t.Fatalf("wrong reply kind consumed TLS request: %v", err)
			default:
			}
			if test.reply != "" {
				reply := messageReply(test.reply, request.ID, base64.RawURLEncoding.EncodeToString(ed25519.Sign(key, message)))
				if test.name == "invalid" {
					reply.SignatureB64 = "AA"
				}
				reply.Signer, reply.Reason = "vault", test.reason
				channel.reply(reply)
				channel.reply(reply)
			}
			if test.name == "eof" {
				channel.done <- struct{}{}
			}
			err = <-result
			if test.reason == "" {
				if err != nil {
					t.Fatal(err)
				}
			} else {
				via := "ck-bus"
				if test.name == "failure" {
					via = "vault"
				}
				if err == nil || err.Error() != "hub TLS signer via "+via+": "+test.reason {
					t.Fatalf("wrong error: %v", err)
				}
			}
			if test.name == "timeout" && (time.Since(started) < 5*time.Second || time.Since(started) > 6*time.Second) {
				t.Fatal("signer did not enforce five-second budget")
			}
			if _, err := signer.Sign(rand.Reader, message, crypto.SHA256); err == nil {
				t.Fatal("Ed25519 accepted a digest")
			}
		})
	}
}

func messageReply(kind string, id uint64, signature string) message {
	return message{Type: kind, ID: &id, SignatureB64: signature}
}
