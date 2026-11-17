package gateway

import (
	"encoding/json"
	"log/slog"
	"net/http"
	"net/http/httputil"
	"net/url"
	"time"
)

// Upstreams holds the base URLs of the services behind the gateway.
type Upstreams struct {
	Users   *url.URL
	Orders  *url.URL
	Billing *url.URL
}

// RegisterRoutes maps the public paths to reverse proxies of the internal services.
func RegisterRoutes(mux *http.ServeMux, upstreams Upstreams) {
	mux.Handle("/v1/users/", httputil.NewSingleHostReverseProxy(upstreams.Users))
	mux.Handle("/v1/orders/", httputil.NewSingleHostReverseProxy(upstreams.Orders))
	mux.Handle("/v1/customers/", httputil.NewSingleHostReverseProxy(upstreams.Orders))
	mux.Handle("/v1/billing/", httputil.NewSingleHostReverseProxy(upstreams.Billing))
	mux.HandleFunc("/healthz", HealthCheckHandler)
}

// HealthCheckHandler reports that the gateway process is up, for the load balancer probe.
func HealthCheckHandler(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
}

type statusRecorder struct {
	http.ResponseWriter
	status int
}

func (s *statusRecorder) WriteHeader(code int) {
	s.status = code
	s.ResponseWriter.WriteHeader(code)
}

// LoggingMiddleware writes one structured access log line per request with method, path,
// status code and duration.
func LoggingMiddleware(logger *slog.Logger, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		started := time.Now()
		recorder := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(recorder, r)
		logger.Info("request",
			"method", r.Method,
			"path", r.URL.Path,
			"status", recorder.status,
			"duration_ms", time.Since(started).Milliseconds(),
		)
	})
}
