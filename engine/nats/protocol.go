package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"sync"
	"unicode/utf8"
)

const maxLineBytes = 65536

type pin struct {
	Kind      string `json:"kind"`
	SHA256Hex string `json:"sha256_hex"`
}

type remote struct {
	Name        string   `json:"name"`
	Account     string   `json:"account"`
	URLs        []string `json:"urls"`
	DenyImports []string `json:"deny_imports"`
	DenyExports []string `json:"deny_exports"`
	Pin         pin      `json:"pin"`
	ServerName  string   `json:"server_name,omitempty"`
}

type hubListener struct {
	Port       uint16 `json:"port"`
	CertDERB64 string `json:"cert_der_b64"`
}

type message struct {
	V            int          `json:"v"`
	Type         string       `json:"type"`
	ID           *uint64      `json:"id,omitempty"`
	Generation   *uint64      `json:"generation,omitempty"`
	Remotes      []remote     `json:"remotes,omitempty"`
	HubListener  *hubListener `json:"hub_listener,omitempty"`
	JWT          string       `json:"jwt,omitempty"`
	SignatureB64 string       `json:"signature_b64,omitempty"`
	Signer       string       `json:"signer,omitempty"`
	Reason       string       `json:"reason,omitempty"`
	DeadlineMS   *uint64      `json:"deadline_ms,omitempty"`
}

type codec struct {
	reader *bufio.Reader
}

func newCodec(r io.Reader) *codec {
	return &codec{reader: bufio.NewReaderSize(r, maxLineBytes)}
}

func (c *codec) read() (message, error) {
	var m message
	line, err := c.reader.ReadSlice('\n')
	if err == io.EOF && len(line) == 0 {
		return m, io.EOF
	}
	if err != nil {
		return m, fmt.Errorf("protocol: incomplete or oversized line: %w", err)
	}
	if len(line) > maxLineBytes || !utf8.Valid(line) {
		return m, fmt.Errorf("protocol: invalid UTF-8 or oversized line")
	}
	var envelope struct {
		V    int    `json:"v"`
		Type string `json:"type"`
	}
	if err := json.Unmarshal(line, &envelope); err != nil || envelope.V != 1 || envelope.Type == "" {
		return m, fmt.Errorf("protocol: expected a v:1 object with a type")
	}
	m.V, m.Type = envelope.V, envelope.Type
	var payload any
	switch m.Type {
	case "state":
		payload = &struct {
			Generation  **uint64      `json:"generation"`
			Remotes     *[]remote     `json:"remotes"`
			HubListener **hubListener `json:"hub_listener"`
		}{&m.Generation, &m.Remotes, &m.HubListener}
	case "shutdown":
		payload = &struct {
			DeadlineMS **uint64 `json:"deadline_ms"`
		}{&m.DeadlineMS}
	case "sign_ok":
		payload = &struct {
			ID           **uint64 `json:"id"`
			JWT          *string  `json:"jwt"`
			SignatureB64 *string  `json:"signature_b64"`
		}{&m.ID, &m.JWT, &m.SignatureB64}
	case "tls_sign_ok":
		payload = &struct {
			ID           **uint64 `json:"id"`
			SignatureB64 *string  `json:"signature_b64"`
		}{&m.ID, &m.SignatureB64}
	case "sign_failed", "tls_sign_failed":
		payload = &struct {
			ID     **uint64 `json:"id"`
			Signer *string  `json:"signer"`
			Reason *string  `json:"reason"`
		}{&m.ID, &m.Signer, &m.Reason}
	default:
		return m, nil // Unknown types are logged once by the receiver and ignored.
	}
	if err := json.Unmarshal(line, payload); err != nil {
		return m, fmt.Errorf("protocol: %w", err)
	}
	switch m.Type {
	case "state":
		if m.Generation == nil || m.Remotes == nil {
			return m, fmt.Errorf("protocol: state requires generation and remotes")
		}
	case "shutdown":
		if m.DeadlineMS == nil || *m.DeadlineMS < 1 || *m.DeadlineMS > 25000 {
			return m, fmt.Errorf("protocol: shutdown deadline_ms must be 1..25000")
		}
	default:
		if m.ID == nil {
			return m, fmt.Errorf("protocol: reply requires an id")
		}
	}
	return m, nil
}

type output struct {
	mu sync.Mutex
	w  io.Writer
}

func (o *output) send(kind string, fields map[string]any) error {
	fields["v"], fields["type"] = 1, kind
	line, err := json.Marshal(fields)
	if err != nil {
		return err
	}
	if len(line)+1 > maxLineBytes {
		return fmt.Errorf("protocol: outgoing line exceeds %d bytes", maxLineBytes)
	}
	o.mu.Lock()
	defer o.mu.Unlock()
	_, err = o.w.Write(append(line, '\n'))
	return err
}
