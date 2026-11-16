package main

import (
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"time"

	"github.com/nats-io/nats-server/v2/logger"
	"github.com/nats-io/nats-server/v2/server"
)

const version = "0.1.0"
const patchID = "reload-signaturecb-1"

type terminal struct {
	code    int
	timeout time.Duration
}

func main() {
	config := flag.String("c", "", "path to server.conf")
	flag.Parse()
	if *config == "" || flag.NArg() != 0 {
		fmt.Fprintln(os.Stderr, "usage: ck-nats-engine -c <server.conf>")
		os.Exit(1)
	}
	os.Exit(run(*config, os.Stdin, os.Stdout))
}

func run(config string, in io.Reader, out io.Writer) int {
	opts, err := server.ProcessConfigFile(config)
	if err != nil {
		log.Print(err)
		return 1
	}
	srv, err := server.NewServer(opts)
	if err != nil {
		log.Print(err)
		return 1
	}
	// The protocol owns stdout; NATS' ordinary logger writes exclusively to stderr.
	srv.SetLogger(logger.NewStdLogger(true, opts.Debug, opts.Trace, false, true), opts.Debug, opts.Trace)
	writer := &output{w: out}
	tracker := newRemoteTracker()
	sign := newSigner(writer, tracker.failed)
	states := make(chan message, 16)
	stop := make(chan terminal, 1)
	done := make(chan struct{})
	defer close(done)
	finish := func(code int, timeout time.Duration) {
		select {
		case stop <- terminal{code, timeout}:
		default:
		}
	}
	go func() {
		codec := newCodec(in)
		unknown := make(map[string]bool)
		for {
			m, err := codec.read()
			if err != nil {
				code := 0
				if err != io.EOF {
					log.Print(err)
					code = 2
				}
				finish(code, 5*time.Second)
				return
			}
			switch m.Type {
			case "state":
				select {
				case states <- m:
				case <-done:
					return
				}
			case "sign_ok", "sign_failed", "tls_sign_ok", "tls_sign_failed":
				sign.reply(m)
			case "shutdown":
				finish(0, min(5*time.Second, time.Duration(*m.DeadlineMS)*time.Millisecond))
				return
			default:
				if !unknown[m.Type] {
					log.Printf("ignored unknown message type %q", m.Type)
					unknown[m.Type] = true
				}
			}
		}
	}()
	go func() {
		go srv.Start()
		if !srv.ReadyForConnections(10*time.Second) || (opts.JetStream && !srv.JetStreamEnabled()) {
			log.Print("NATS failed to become ready")
			finish(1, 5*time.Second)
			return
		}
		if err := writer.send("ready", map[string]any{"version": version, "nats_version": server.VERSION, "patch": patchID}); err != nil {
			finish(1, 5*time.Second)
			return
		}
		if err := serve(config, srv, opts.JetStream, states, writer, sign, tracker, done); err != nil {
			log.Print(err)
			finish(1, 5*time.Second)
		}
	}()
	end := <-stop
	close(sign.done)
	stopped := make(chan struct{})
	go func() {
		srv.Shutdown()
		srv.WaitForShutdown()
		close(stopped)
	}()
	select {
	case <-stopped:
	case <-time.After(end.timeout):
		log.Print("NATS shutdown deadline reached")
	}
	return end.code
}

func serve(config string, srv *server.Server, jetstream bool, states <-chan message, out *output, sign *signer, tracker *remoteTracker, done <-chan struct{}) error {
	var remotes []remote
	var generation uint64
	var applied bool
	var lastStatus string
	lastSent := time.Time{}
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	publish := func() error {
		status := tracker.status(srv, jetstream, remotes)
		encoded := statusFingerprint(status)
		if encoded != lastStatus || time.Since(lastSent) >= 5*time.Second {
			if err := out.send("status", status); err != nil {
				return err
			}
			lastStatus, lastSent = encoded, time.Now()
		}
		return nil
	}
	if err := publish(); err != nil {
		return err
	}
	for {
		select {
		case m := <-states:
			g := *m.Generation
			if applied && g <= generation {
				if err := out.send("applied", map[string]any{"generation": g}); err != nil {
					return err
				}
				continue
			}
			opts, err := server.ProcessConfigFile(config)
			if err == nil && m.HubListener != nil {
				// Hub listener certificates and TLS signing over the protocol are not implemented yet.
				err = fmt.Errorf("hub listener is not implemented")
			}
			if err == nil {
				opts.LeafNode.Remotes, err = buildRemotes(m.Remotes, remotes, func(name string, nonce []byte) (string, []byte, error) {
					tracker.signing(name, nonce)
					return sign.sign(name, nonce)
				}, tracker.failed)
			}
			if err == nil {
				err = srv.ReloadOptions(opts)
			}
			if err != nil {
				if err := out.send("apply_failed", map[string]any{"generation": g, "error": err.Error()}); err != nil {
					return err
				}
				continue
			}
			remotes, generation, applied, jetstream = m.Remotes, g, true, opts.JetStream
			tracker.retain(remotes)
			if err := out.send("applied", map[string]any{"generation": g}); err != nil {
				return err
			}
			if err := publish(); err != nil {
				return err
			}
		case <-ticker.C:
			if err := publish(); err != nil {
				return err
			}
		case <-done:
			return nil
		}
	}
}
