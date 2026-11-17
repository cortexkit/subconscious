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
	pending  map[uint64]signRequest
	sendGate chan struct{}
	out      *output
	done     chan struct{}
	fail     func(string, error)
}

type signRequest struct {
	kind  string
	reply chan message
}

func newSigner(out *output, fail func(string, error)) *signer {
	return &signer{pending: make(map[uint64]signRequest), sendGate: make(chan struct{}, 1), out: out, done: make(chan struct{}), fail: fail}
}

func (s *signer) reply(m message) {
	s.mu.Lock()
	request, found := s.pending[*m.ID]
	matched := found && (m.Type == request.kind+"_ok" || m.Type == request.kind+"_failed")
	if matched {
		delete(s.pending, *m.ID)
	}
	s.mu.Unlock()
	if !matched {
		log.Printf("ignored %s for unknown or answered id %d", m.Type, *m.ID)
		return
	}
	request.reply <- m
}

func (s *signer) sign(remote string, nonce []byte) (jwt string, signature []byte, err error) {
	defer func() {
		if err != nil {
			s.fail(remote, err)
		}
	}()
	m, err := s.request("sign", "leaf signer", map[string]any{"remote": remote, "nonce_b64": string(nonce)})
	if err != nil {
		return "", nil, err
	}
	sig, err := base64.RawURLEncoding.Strict().DecodeString(m.SignatureB64)
	if err != nil || len(sig) != 64 || m.JWT == "" {
		return "", nil, fmt.Errorf("leaf signer via ck-bus: invalid signature reply")
	}
	return m.JWT, sig, nil
}

func (s *signer) request(kind, label string, fields map[string]any) (message, error) {
	failure := func(reason string) (message, error) {
		return message{}, fmt.Errorf("%s via ck-bus: %s", label, reason)
	}
	timer := time.NewTimer(5 * time.Second)
	defer timer.Stop()
	// Allocate ids in pipe-write order, including concurrent reconnect callbacks.
	select {
	case s.sendGate <- struct{}{}:
	case <-timer.C:
		return failure("timed out")
	case <-s.done:
		return failure("wrapper gone")
	}
	s.mu.Lock()
	s.nextID++
	id := s.nextID
	ch := make(chan message, 1)
	s.pending[id] = signRequest{kind, ch}
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		delete(s.pending, id)
		s.mu.Unlock()
	}()
	// Include the pipe write in the five-second budget even if the wrapper stops reading.
	written := make(chan error, 1)
	fields["id"] = id
	go func() {
		written <- s.out.send(kind, fields)
		<-s.sendGate
	}()
	for {
		select {
		case err := <-written:
			if err != nil {
				return failure(err.Error())
			}
		case m := <-ch:
			if m.Type == kind+"_failed" {
				return message{}, fmt.Errorf("%s via %s: %s", label, m.Signer, m.Reason)
			}
			return m, nil
		case <-timer.C:
			return failure("timed out")
		case <-s.done:
			return failure("wrapper gone")
		}
	}
}
