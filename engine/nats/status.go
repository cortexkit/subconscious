package main

import (
	"encoding/json"
	"sync"
	"time"

	"github.com/nats-io/nats-server/v2/server"
)

type leafStatus struct {
	Remote  string `json:"remote"`
	State   string `json:"state"`
	URL     string `json:"url,omitempty"`
	SinceMS int64  `json:"since_ms"`
	Error   string `json:"error,omitempty"`
}

type remoteTracker struct {
	mu     sync.Mutex
	errors map[string]string
	nonces map[string]string
	last   map[string]leafStatus
}

func newRemoteTracker() *remoteTracker {
	return &remoteTracker{errors: make(map[string]string), nonces: make(map[string]string), last: make(map[string]leafStatus)}
}

func (t *remoteTracker) signing(name string, nonce []byte) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.nonces[name] = string(nonce)
	delete(t.errors, name)
}

func (t *remoteTracker) failed(name string, err error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.errors[name] = err.Error()
}

func (t *remoteTracker) retain(remotes []remote) {
	t.mu.Lock()
	defer t.mu.Unlock()
	keep := make(map[string]bool)
	for _, r := range remotes {
		keep[r.Name] = true
	}
	for name := range t.last {
		if !keep[name] {
			delete(t.last, name)
			delete(t.errors, name)
			delete(t.nonces, name)
		}
	}
}

func (t *remoteTracker) status(srv *server.Server, js bool, remotes []remote) map[string]any {
	health := srv.Healthz(&server.HealthzOptions{JSEnabledOnly: true})
	jetstream := map[string]any{"enabled": js, "ok": health.Status == "ok"}
	if health.Error != "" {
		jetstream["error"] = health.Error
	}
	leafz, _ := srv.Leafz(nil)
	connected := make(map[string]bool)
	if leafz != nil {
		for _, leaf := range leafz.Leafs {
			if c := srv.GetLeafNode(leaf.ID); c != nil {
				connected[string(c.GetNonce())] = true
			}
		}
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	leafs := make([]leafStatus, 0, len(remotes))
	for _, r := range remotes {
		state := leafStatus{Remote: r.Name, State: "connecting"}
		if nonce, ok := t.nonces[r.Name]; ok && connected[nonce] {
			state.State = "connected"
			delete(t.errors, r.Name)
		} else if reason := t.errors[r.Name]; reason != "" {
			state.State, state.Error = "failed", reason
		}
		old := t.last[r.Name]
		if old.State == state.State && old.Error == state.Error {
			state.SinceMS = old.SinceMS
		} else {
			state.SinceMS = time.Now().UnixMilli()
		}
		t.last[r.Name] = state
		leafs = append(leafs, state)
	}
	return map[string]any{"jetstream": jetstream, "leafs": leafs}
}

func statusFingerprint(status map[string]any) string {
	b, _ := json.Marshal(status)
	return string(b)
}
