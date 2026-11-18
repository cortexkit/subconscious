package main

import (
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"fmt"
	"net"
	"net/url"
	"reflect"
	"regexp"
	"strconv"
	"time"

	"github.com/nats-io/nats-server/v2/server"
)

var remoteName = regexp.MustCompile(`^[a-z0-9_-]{1,64}$`)
var pinHex = regexp.MustCompile(`^[0-9a-f]{64}$`)

func remoteTLS(r remote, failed func(error)) (*tls.Config, error) {
	if !pinHex.MatchString(r.Pin.SHA256Hex) {
		return nil, fmt.Errorf("remote %q: pin must be 64 lowercase hex characters", r.Name)
	}
	if r.Pin.Kind != "spki" && r.Pin.Kind != "ca_spki" {
		return nil, fmt.Errorf("remote %q: unknown pin kind %q", r.Name, r.Pin.Kind)
	}
	if (r.Pin.Kind == "ca_spki") != (r.ServerName != "") {
		return nil, fmt.Errorf("remote %q: server_name is required only for ca_spki", r.Name)
	}
	want, _ := hex.DecodeString(r.Pin.SHA256Hex)
	verify := func(certs []*x509.Certificate) error {
		if len(certs) == 0 {
			return fmt.Errorf("remote %q: peer presented no certificate", r.Name)
		}
		matches := func(cert *x509.Certificate) bool {
			sum := sha256.Sum256(cert.RawSubjectPublicKeyInfo)
			return string(sum[:]) == string(want)
		}
		if r.Pin.Kind == "spki" {
			if !matches(certs[0]) {
				return fmt.Errorf("remote %q: certificate SPKI pin mismatch", r.Name)
			}
			return nil
		}
		intermediates := x509.NewCertPool()
		for _, cert := range certs[1:] {
			intermediates.AddCert(cert)
		}
		for _, root := range certs[1:] {
			if !matches(root) {
				continue
			}
			roots := x509.NewCertPool()
			roots.AddCert(root)
			_, err := certs[0].Verify(x509.VerifyOptions{
				Roots: roots, Intermediates: intermediates, DNSName: r.ServerName, CurrentTime: time.Now(),
			})
			return err
		}
		return fmt.Errorf("remote %q: pinned CA was not presented", r.Name)
	}
	return &tls.Config{
		MinVersion: tls.VersionTLS12,
		ServerName: r.ServerName,
		// Verification is entirely pin-based; hosted hubs also require a chain and DNS name.
		InsecureSkipVerify: true,
		// VerifyConnection also runs when TLS resumes a session, so every connection checks its pin.
		VerifyConnection: func(cs tls.ConnectionState) error {
			err := verify(cs.PeerCertificates)
			if err != nil && failed != nil {
				failed(err)
			}
			return err
		},
	}, nil
}

func buildRemotes(remotes []remote, previous []remote, sign func(string, []byte) (string, []byte, error), failed func(string, error)) ([]*server.RemoteLeafOpts, error) {
	result := make([]*server.RemoteLeafOpts, 0, len(remotes))
	names := make(map[string]bool)
	for _, r := range remotes {
		if !remoteName.MatchString(r.Name) || names[r.Name] {
			return nil, fmt.Errorf("invalid or duplicate remote name %q", r.Name)
		}
		names[r.Name] = true
		for _, old := range previous {
			if old.Name == r.Name && (old.Account != r.Account || !reflect.DeepEqual(old.DenyImports, r.DenyImports) || !reflect.DeepEqual(old.DenyExports, r.DenyExports) || old.Pin != r.Pin || old.ServerName != r.ServerName) {
				return nil, fmt.Errorf("remote %q: account, deny lists or pin changes require a new name", r.Name)
			}
		}
		if r.Account == "" || len(r.URLs) < 1 || len(r.URLs) > 16 {
			return nil, fmt.Errorf("remote %q: account and 1..16 URLs required", r.Name)
		}
		for _, subjects := range [][]string{r.DenyImports, r.DenyExports} {
			for _, subject := range subjects {
				if !server.IsValidSubject(subject) {
					return nil, fmt.Errorf("remote %q: invalid deny subject %q", r.Name, subject)
				}
			}
		}
		tlsConfig, err := remoteTLS(r, func(err error) { failed(r.Name, err) })
		if err != nil {
			return nil, err
		}
		urls := make([]*url.URL, 0, len(r.URLs))
		for _, address := range r.URLs {
			u, err := url.Parse(address)
			if err != nil || u.Scheme != "tls" || u.User != nil || u.Path != "" || u.RawQuery != "" || u.Fragment != "" {
				return nil, fmt.Errorf("remote %q: expected tls://host:port", r.Name)
			}
			host, port, err := net.SplitHostPort(u.Host)
			p, portErr := strconv.Atoi(port)
			if err != nil || host == "" || portErr != nil || p < 1 || p > 65535 {
				return nil, fmt.Errorf("remote %q: invalid host or port", r.Name)
			}
			urls = append(urls, u)
		}
		result = append(result, &server.RemoteLeafOpts{
			LocalAccount: r.Account, URLs: urls, NoRandomize: true,
			// NATS includes Nkey in reload identity, but SignatureCB bypasses seed handling.
			Nkey:        "ck-bus:" + r.Name,
			SignatureCB: func(nonce []byte) (string, []byte, error) { return sign(r.Name, nonce) },
			// Send TLS first so relays can forward the leaf's first bytes without plaintext INFO.
			TLS: true, TLSConfig: tlsConfig, TLSHandshakeFirst: true,
			DenyImports: append([]string(nil), r.DenyImports...), DenyExports: append([]string(nil), r.DenyExports...),
		})
	}
	return result, nil
}
