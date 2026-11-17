// Package gateway is the public HTTP entry point in front of the storefront services.
package gateway

import (
	"net/http"
	"strconv"
	"sync"
	"time"
)

// TokenBucket allows bursts of up to Capacity requests and refills at Rate tokens per second.
type TokenBucket struct {
	mu       sync.Mutex
	Capacity float64
	Rate     float64
	tokens   float64
	updated  time.Time
}

// NewTokenBucket returns a full bucket.
func NewTokenBucket(capacity, rate float64) *TokenBucket {
	return &TokenBucket{Capacity: capacity, Rate: rate, tokens: capacity, updated: time.Now()}
}

// Allow takes one token when available. It refills the bucket for the time passed since the
// last call first.
func (b *TokenBucket) Allow(now time.Time) bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	elapsed := now.Sub(b.updated).Seconds()
	b.tokens = min(b.Capacity, b.tokens+elapsed*b.Rate)
	b.updated = now
	if b.tokens < 1 {
		return false
	}
	b.tokens--
	return true
}

// RateLimitMiddleware answers 429 Too Many Requests with a Retry-After header when the client's
// bucket is empty. Clients are told apart by their API key, else by remote address.
func RateLimitMiddleware(buckets *sync.Map, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		client := r.Header.Get("X-Api-Key")
		if client == "" {
			client = r.RemoteAddr
		}
		value, _ := buckets.LoadOrStore(client, NewTokenBucket(20, 10))
		if !value.(*TokenBucket).Allow(time.Now()) {
			w.Header().Set("Retry-After", strconv.Itoa(1))
			http.Error(w, "too many requests", http.StatusTooManyRequests)
			return
		}
		next.ServeHTTP(w, r)
	})
}
