// Independent dynamic /ok service for validating the arrival generator.
// This is deliberately excluded from cross-language performance rankings.
package main

import (
	"context"
	"flag"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"
)

func main() {
	address := flag.String("address", "127.0.0.1:19090", "loopback diagnostic address")
	flag.Parse()
	mux := http.NewServeMux()
	mux.HandleFunc("/ok", func(w http.ResponseWriter, r *http.Request) {
		rid := r.URL.Query().Get("rid")
		if r.Method != "GET" || len(rid) < 1 || len(rid) > 16 {
			http.Error(w, "invalid request", 400)
			return
		}
		for _, c := range rid {
			if c < '0' || c > '9' {
				http.Error(w, "invalid request", 400)
				return
			}
		}
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		_, _ = fmt.Fprint(w, "ok:"+rid)
	})
	mux.HandleFunc("/metrics", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = fmt.Fprint(w, `{"active":0,"waiting_timer":0,"waiting_tcp":0,"actual_cpu_jobs":0}`)
	})
	server := &http.Server{Addr: *address, Handler: mux, ReadHeaderTimeout: 5 * time.Second, IdleTimeout: 5 * time.Second, MaxHeaderBytes: 32768}
	stopped := make(chan os.Signal, 1)
	signal.Notify(stopped, syscall.SIGTERM, os.Interrupt)
	go func() {
		<-stopped
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = server.Shutdown(ctx)
	}()
	if err := server.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
