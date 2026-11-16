package main

import (
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"math/big"
	"strconv"
	"sync/atomic"
	"testing"
	"time"

	"github.com/nats-io/nats-server/v2/server"
)

func certificate(t *testing.T, parent *x509.Certificate, parentKey ed25519.PrivateKey, ca bool) (tls.Certificate, *x509.Certificate, ed25519.PrivateKey) {
	t.Helper()
	pub, key, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	template := &x509.Certificate{
		SerialNumber: big.NewInt(time.Now().UnixNano()), DNSNames: []string{"hub.example.test"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(time.Hour),
		BasicConstraintsValid: true, IsCA: ca,
		KeyUsage:    x509.KeyUsageDigitalSignature,
		ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}
	if ca {
		template.KeyUsage |= x509.KeyUsageCertSign
	}
	if parent == nil {
		parent, parentKey = template, key
	}
	der, err := x509.CreateCertificate(rand.Reader, template, parent, pub, parentKey)
	if err != nil {
		t.Fatal(err)
	}
	cert, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	return tls.Certificate{Certificate: [][]byte{der}, PrivateKey: key, Leaf: cert}, cert, key
}

func certPin(cert *x509.Certificate) string {
	sum := sha256.Sum256(cert.RawSubjectPublicKeyInfo)
	return hex.EncodeToString(sum[:])
}

func sampleRemote(pinValue string) remote {
	return remote{Name: "hub-1", Account: "$G", URLs: []string{"tls://127.0.0.1:7422"},
		DenyImports: []string{"fedlocal.>", "fedout.>"}, DenyExports: []string{"fedlocal.>", "fedout.>"},
		Pin: pin{Kind: "spki", SHA256Hex: pinValue}}
}

func TestRemotePinVerification(t *testing.T) {
	_, root, rootKey := certificate(t, nil, nil, true)
	_, intermediate, intermediateKey := certificate(t, root, rootKey, true)
	_, leaf, _ := certificate(t, intermediate, intermediateKey, false)
	_, other, _ := certificate(t, nil, nil, true)
	for _, tc := range []struct {
		name       string
		pin        pin
		serverName string
		chain      []*x509.Certificate
		wantOK     bool
	}{
		{"spki accepts without name or chain", pin{"spki", certPin(leaf)}, "", []*x509.Certificate{leaf}, true},
		{"spki rejects wrong key", pin{"spki", certPin(other)}, "", []*x509.Certificate{leaf}, false},
		{"spki rejects empty chain", pin{"spki", certPin(leaf)}, "", nil, false},
		{"hosted accepts pinned root chain", pin{"ca_spki", certPin(root)}, "hub.example.test", []*x509.Certificate{leaf, intermediate, root}, true},
		{"hosted accepts pinned intermediate", pin{"ca_spki", certPin(intermediate)}, "hub.example.test", []*x509.Certificate{leaf, intermediate}, true},
		{"hosted requires presented pinned CA", pin{"ca_spki", certPin(root)}, "hub.example.test", []*x509.Certificate{leaf, intermediate}, false},
		{"hosted rejects wrong pin", pin{"ca_spki", certPin(other)}, "hub.example.test", []*x509.Certificate{leaf, intermediate, root}, false},
		{"hosted rejects wrong DNS name", pin{"ca_spki", certPin(root)}, "wrong.example.test", []*x509.Certificate{leaf, intermediate, root}, false},
		{"hosted rejects unrelated leaf", pin{"ca_spki", certPin(root)}, "hub.example.test", []*x509.Certificate{other, root}, false},
		{"hosted never pins leaf as root", pin{"ca_spki", certPin(leaf)}, "hub.example.test", []*x509.Certificate{leaf}, false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			r := sampleRemote(tc.pin.SHA256Hex)
			r.Pin, r.ServerName = tc.pin, tc.serverName
			cfg, err := remoteTLS(r, nil)
			if err != nil {
				t.Fatal(err)
			}
			if err := cfg.VerifyConnection(tls.ConnectionState{PeerCertificates: tc.chain}); (err == nil) != tc.wantOK {
				t.Fatalf("verification: %v, want success %v", err, tc.wantOK)
			}
		})
	}
}

func TestRemoteVerifyConnectionRejectsResumedPinMismatch(t *testing.T) {
	_, leaf, _ := certificate(t, nil, nil, false)
	_, other, _ := certificate(t, nil, nil, false)
	var failure error
	cfg, err := remoteTLS(sampleRemote(certPin(other)), func(err error) { failure = err })
	if err != nil {
		t.Fatal(err)
	}
	if cfg.VerifyConnection == nil {
		t.Fatal("pin verification must run through VerifyConnection, including resumed sessions")
	}
	err = cfg.VerifyConnection(tls.ConnectionState{PeerCertificates: []*x509.Certificate{leaf}, DidResume: true})
	if err == nil {
		t.Fatal("resumed connection accepted a mismatched SPKI pin")
	}
	if failure != err {
		t.Fatalf("failed callback got %v, want verification error %v", failure, err)
	}
}

func TestRemoteChangesRequireRename(t *testing.T) {
	_, cert, _ := certificate(t, nil, nil, false)
	old := sampleRemote(certPin(cert))
	for _, change := range []func(*remote){
		func(r *remote) { r.Pin.SHA256Hex = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
		func(r *remote) { r.DenyImports = []string{"changed.>"} },
		func(r *remote) { r.DenyExports = []string{"changed.>"} },
		func(r *remote) { r.Account = "OTHER" },
	} {
		r := old
		change(&r)
		if _, err := buildRemotes([]remote{r}, []remote{old}, nil, func(string, error) {}); err == nil {
			t.Fatal("accepted in-place identity change")
		}
		r.Name = "hub-2"
		opts, err := buildRemotes([]remote{r}, []remote{old}, nil, func(string, error) {})
		if err != nil || opts[0].Nkey != "ck-bus:hub-2" {
			t.Fatalf("renamed remote: %+v %v", opts, err)
		}
	}
}

func startServer(t *testing.T, opts *server.Options) *server.Server {
	t.Helper()
	opts.NoLog, opts.NoSigs = true, true
	s, err := server.NewServer(opts)
	if err != nil {
		t.Fatal(err)
	}
	go s.Start()
	t.Cleanup(s.Shutdown)
	if !s.ReadyForConnections(5 * time.Second) {
		t.Fatal("server not ready")
	}
	return s
}

func eventually(t *testing.T, why string, fn func() bool) {
	t.Helper()
	deadline := time.Now().Add(4 * time.Second)
	for time.Now().Before(deadline) {
		if fn() {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal(why)
}

func TestRemoteReloadSyntheticNkeyAndPinRename(t *testing.T) {
	cert1, x509Cert1, key := certificate(t, nil, nil, false)
	cert2, x509Cert2, _ := certificate(t, nil, nil, false)
	hubOptions := func(port int, cert tls.Certificate) *server.Options {
		return &server.Options{Host: "127.0.0.1", Port: -1, LeafNode: server.LeafNodeOpts{
			Host: "127.0.0.1", Port: port, NoAdvertise: true,
			TLSConfig: &tls.Config{Certificates: []tls.Certificate{cert}, MinVersion: tls.VersionTLS12},
		}}
	}
	hub := startServer(t, hubOptions(-1, cert1))
	v, err := hub.Varz(nil)
	if err != nil {
		t.Fatal(err)
	}
	r := sampleRemote(certPin(x509Cert1))
	r.URLs = []string{"tls://127.0.0.1:" + strconv.Itoa(v.LeafNode.Port)}
	var signs atomic.Int32
	sign := func(_ string, nonce []byte) (string, []byte, error) {
		signs.Add(1)
		return "", ed25519.Sign(key, nonce), nil
	}
	var pinFailures atomic.Int32
	failed := func(string, error) { pinFailures.Add(1) }
	options := func(current remote, previous []remote) *server.Options {
		t.Helper()
		remotes, err := buildRemotes([]remote{current}, previous, sign, failed)
		if err != nil {
			t.Fatal(err)
		}
		return &server.Options{Host: "127.0.0.1", Port: -1, NoLog: true, NoSigs: true,
			LeafNode: server.LeafNodeOpts{Remotes: remotes, ReconnectInterval: 50 * time.Millisecond}}
	}
	leaf := startServer(t, options(r, nil))
	eventually(t, "synthetic Nkey did not authenticate through callback", func() bool { return leaf.NumLeafNodes() == 1 && signs.Load() > 0 })
	if err := leaf.ReloadOptions(options(r, []remote{r})); err != nil {
		t.Fatalf("unchanged callback remote reload: %v", err)
	}
	// Force reconnect to exercise the callback again, not seed parsing on the synthetic Nkey.
	leafz, _ := hub.Leafz(nil)
	if len(leafz.Leafs) != 1 {
		t.Fatal("missing hub connection")
	}
	before := signs.Load()
	hub.Shutdown()
	hub.WaitForShutdown()
	hub = startServer(t, hubOptions(v.LeafNode.Port, cert1))
	eventually(t, "reconnect did not use callback", func() bool { return leaf.NumLeafNodes() == 1 && signs.Load() > before })
	old := r
	r.Name, r.Pin.SHA256Hex = "hub-2", certPin(x509Cert2)
	before = signs.Load()
	if err := leaf.ReloadOptions(options(r, []remote{old})); err != nil {
		t.Fatalf("pin-only renamed reload: %v", err)
	}
	eventually(t, "old remote stayed connected or new pin was not enforced", func() bool { return leaf.NumLeafNodes() == 0 && pinFailures.Load() > 0 })
	if signs.Load() != before {
		t.Fatal("wrong pin reached signer")
	}
	hub.Shutdown()
	hub.WaitForShutdown()
	hub = startServer(t, hubOptions(v.LeafNode.Port, cert2))
	eventually(t, "new pin did not replace old remote", func() bool { return leaf.NumLeafNodes() == 1 && signs.Load() > before })
	if hub.NumLeafNodes() != 1 {
		t.Fatal("new remote missing at hub")
	}
}
