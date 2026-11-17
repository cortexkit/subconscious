package main

import (
	"crypto"
	"crypto/ed25519"
	"crypto/tls"
	"crypto/x509"
	"encoding/base64"
	"fmt"
	"io"

	"github.com/nats-io/nats-server/v2/server"
)

// The certificate is public; the private key stays behind the wrapper's channel.
type hubSigner struct {
	public  ed25519.PublicKey
	channel *signer
}

func (s *hubSigner) Public() crypto.PublicKey { return s.public }

func (s *hubSigner) Sign(_ io.Reader, message []byte, opts crypto.SignerOpts) ([]byte, error) {
	if opts.HashFunc() != crypto.Hash(0) {
		return nil, fmt.Errorf("hub TLS signer via ck-bus: Ed25519 requires the whole message")
	}
	reply, err := s.channel.request("tls_sign", "hub TLS signer", map[string]any{
		"message_b64": base64.RawURLEncoding.EncodeToString(message), "scheme": "ed25519",
	})
	if err != nil {
		return nil, err
	}
	sig, err := base64.RawURLEncoding.Strict().DecodeString(reply.SignatureB64)
	if err != nil || len(sig) != ed25519.SignatureSize {
		return nil, fmt.Errorf("hub TLS signer via ck-bus: invalid signature reply")
	}
	return sig, nil
}

func hubTLS(listener *hubListener, sign *signer) (*tls.Config, error) {
	if listener.Port == 0 {
		return nil, fmt.Errorf("hub listener port must be nonzero")
	}
	der, err := base64.RawURLEncoding.Strict().DecodeString(listener.CertDERB64)
	if err != nil {
		return nil, fmt.Errorf("hub listener certificate: %w", err)
	}
	cert, err := x509.ParseCertificate(der)
	if err != nil {
		return nil, fmt.Errorf("hub listener certificate: %w", err)
	}
	public, ok := cert.PublicKey.(ed25519.PublicKey)
	if !ok {
		return nil, fmt.Errorf("hub listener certificate must use Ed25519")
	}
	return &tls.Config{
		MinVersion: tls.VersionTLS13, MaxVersion: tls.VersionTLS13,
		Certificates: []tls.Certificate{{Certificate: [][]byte{der}, Leaf: cert, PrivateKey: &hubSigner{public, sign}}},
	}, nil
}

func configureHub(opts *server.Options, listener *hubListener, config *tls.Config) {
	opts.LeafNode.Port = int(listener.Port)
	opts.LeafNode.TLSConfig = config
}
