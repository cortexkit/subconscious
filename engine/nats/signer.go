package main

import (
	"encoding/base64"
	"fmt"
	"log"
	"sync"
	"time"
)

type signer struct {
	mu       sync.Mutex
	nextID   uint64
	pending  map[uint64]chan message
	sendGate chan struct{}
	out      *output
	done     chan struct{}
	fail     func(string, error)
}

func newSigner(out *output, fail func(string, error)) *signer {
	return &signer{pending: make(map[uint64]chan message), sendGate: make(chan struct{}, 1), out: out, done: make(chan struct{}), fail: fail}
}

func (s *signer) reply(m message) {
	s.mu.Lock()
	ch := s.pending[*m.ID]
	if m.Type == "sign_ok" || m.Type == "sign_failed" {
		delete(s.pending, *m.ID)
	}
	s.mu.Unlock()
	if ch == nil || (m.Type != "sign_ok" && m.Type != "sign_failed") {
		// Hub TLS signing is decoded, but no hub listener is implemented yet.
		log.Printf("ignored %s for unknown or answered id %d", m.Type, *m.ID)
		return
	}
	ch <- m
}

func (s *signer) sign(remote string, nonce []byte) (jwt string, signature []byte, err error) {
	defer func() {
		if err != nil {
			s.fail(remote, err)
		}
	}()
	timer := time.NewTimer(5 * time.Second)
	defer timer.Stop()
	// Allocate ids in pipe-write order, including concurrent reconnect callbacks.
	select {
	case s.sendGate <- struct{}{}:
	case <-timer.C:
		return "", nil, fmt.Errorf("leaf signer via ck-bus: timed out")
	case <-s.done:
		return "", nil, fmt.Errorf("leaf signer via ck-bus: wrapper gone")
	}
	s.mu.Lock()
	s.nextID++
	id := s.nextID
	ch := make(chan message, 1)
	s.pending[id] = ch
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		delete(s.pending, id)
		s.mu.Unlock()
	}()
	// Include the pipe write in the five-second budget even if the wrapper stops reading.
	written := make(chan error, 1)
	go func() {
		written <- s.out.send("sign", map[string]any{"id": id, "remote": remote, "nonce_b64": string(nonce)})
		<-s.sendGate
	}()
	for {
		select {
		case err := <-written:
			if err != nil {
				return "", nil, fmt.Errorf("leaf signer via ck-bus: %w", err)
			}
		case m := <-ch:
			if m.Type == "sign_failed" {
				return "", nil, fmt.Errorf("leaf signer via %s: %s", m.Signer, m.Reason)
			}
			sig, err := base64.RawURLEncoding.Strict().DecodeString(m.SignatureB64)
			if err != nil || len(sig) != 64 || m.JWT == "" {
				return "", nil, fmt.Errorf("leaf signer via ck-bus: invalid signature reply")
			}
			return m.JWT, sig, nil
		case <-timer.C:
			return "", nil, fmt.Errorf("leaf signer via ck-bus: timed out")
		case <-s.done:
			return "", nil, fmt.Errorf("leaf signer via ck-bus: wrapper gone")
		}
	}
}
