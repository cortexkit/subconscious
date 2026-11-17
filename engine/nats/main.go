package main

import (
	"crypto/tls"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"time"

	"github.com/nats-io/nats-server/v2/logger"
	"github.com/nats-io/nats-server/v2/server"
)

const version = "0.1.2"
const patchID = "reload-signaturecb-1"

type terminal struct {
	code    int
	timeout time.Duration
}

func main() {
	for _, arg := range os.Args[1:] {
		if arg == "--version" {
			fmt.Printf("ck-nats-engine %s\n", version)
			return
		}
	}
	flag.Usage = func() {
		fmt.Fprintln(os.Stderr, "usage: ck-nats-engine --version | -c <server.conf> [--hub]")
		flag.PrintDefaults()
	}
	config := flag.String("c", "", "path to server.conf")
	hub := flag.Bool("hub", false, "wait for hub listener state before starting")
	flag.Parse()
	if *config == "" || flag.NArg() != 0 {
		flag.Usage()
		os.Exit(1)
	}
	os.Exit(run(*config, *hub, os.Stdin, os.Stdout))
}

func run(config string, hub bool, in io.Reader, out io.Writer) int {
	var opts *server.Options
	var srv *server.Server
	var err error
	if !hub {
		opts, err = server.ProcessConfigFile(config)
		if err != nil {
			log.Print(err)
			return 1
		}
	}
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
	var initial *message
	var listenerTLS *tls.Config
	if hub {
		// NATS cannot add a leaf listener by reload. Do not open any server port
		// until the watch supplies the certificate and port for this process.
		// An idle computer has no hub configuration; ck-bus creates it at setup.
		var generation uint64
		var applied bool
		for initial == nil {
			select {
			case end := <-stop:
				close(sign.done)
				return end.code
			case m := <-states:
				g := *m.Generation
				if applied && g <= generation || m.HubListener == nil {
					if !applied || g > generation {
						generation, applied = g, true
					}
					err = writer.send("applied", map[string]any{"generation": g})
				} else {
					listenerTLS, err = hubTLS(m.HubListener, sign)
					if err == nil {
						opts, err = server.ProcessConfigFile(config)
					}
					if err == nil {
						configureHub(opts, m.HubListener, listenerTLS)
						srv, err = server.NewServer(opts)
					}
					if err != nil {
						err = writer.send("apply_failed", map[string]any{"generation": g, "error": err.Error()})
					} else {
						initial = &m
					}
				}
				if err != nil {
					log.Print(err)
					close(sign.done)
					return 1
				}
			}
		}
	}
	if !hub {
		srv, err = server.NewServer(opts)
		if err != nil {
			log.Print(err)
			close(sign.done)
			return 1
		}
	}
	// The protocol owns stdout; NATS' ordinary logger writes exclusively to stderr.
	srv.SetLogger(logger.NewStdLogger(true, opts.Debug, opts.Trace, false, true), opts.Debug, opts.Trace)
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
		if err := serve(config, srv, opts.JetStream, initial, listenerTLS, states, writer, sign, tracker, done); err != nil {
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

func serve(config string, srv *server.Server, jetstream bool, initial *message, listenerTLS *tls.Config, states <-chan message, out *output, sign *signer, tracker *remoteTracker, done <-chan struct{}) error {
	var remotes []remote
	var generation uint64
	var applied bool
	var listener *hubListener
	if initial != nil {
		listener, generation, applied = initial.HubListener, *initial.Generation, true
		if err := out.send("applied", map[string]any{"generation": generation}); err != nil {
			return err
		}
	}
	var lastStatus string
	lastSent := time.Time{}
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	publish := func() error {
		status := tracker.status(srv, jetstream, remotes)
		if listener != nil {
			// NATS readiness includes a successful bind of its configured leaf listener.
			status["listener"] = map[string]any{"port": listener.Port, "bound": srv.ReadyForConnections(time.Millisecond)}
		}
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
			if err == nil {
				switch {
				case listener == nil && m.HubListener != nil:
					err = fmt.Errorf("hub listener requires --hub")
				case listener != nil && (m.HubListener == nil || *m.HubListener != *listener):
					err = fmt.Errorf("hub listener port or certificate changed; restart required")
				case listener != nil:
					configureHub(opts, listener, listenerTLS)
				}
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
