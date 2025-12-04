package server

import (
	"crypto/rand"
	"encoding/base64"
	"net/http"
	"sync"
	"time"

	"github.com/kurodaze/togewire/internal/config"

	"github.com/gin-gonic/gin"
	"golang.org/x/crypto/bcrypt"
)

const (
	sessionCookieName = "togewire_session"
	sessionDuration   = 7 * 24 * time.Hour
	maxLoginAttempts  = 5
	rateLimitWindow   = 30 * time.Minute
)

type session struct {
	token     string
	createdAt time.Time
}

type loginAttempt struct {
	count    int
	firstTry time.Time
}

var (
	sessions      = make(map[string]*session)
	sessionsMu    sync.RWMutex
	loginAttempts = make(map[string]*loginAttempt)
	attemptsMu    sync.RWMutex
	setupToken    string
	setupTokenMu  sync.RWMutex
)

func init() {
	// Cleanup expired sessions and rate limit entries every hour
	go func() {
		for {
			time.Sleep(1 * time.Hour)
			cleanupExpiredSessions()
			cleanupExpiredAttempts()
		}
	}()
}

// middleware checks if user is authenticated
func requireAuth() gin.HandlerFunc {
	return func(c *gin.Context) {
		cfg := config.Get()

		if !cfg.HasAdminPassword() {
			c.Redirect(http.StatusFound, "/admin/login")
			c.Abort()
			return
		}

		// Check session cookie
		sessionToken, err := c.Cookie(sessionCookieName)
		if err != nil {
			c.Redirect(http.StatusFound, "/admin/login")
			c.Abort()
			return
		}

		// Validate session
		sessionsMu.RLock()
		sess, exists := sessions[sessionToken]
		sessionsMu.RUnlock()

		if !exists || time.Since(sess.createdAt) > sessionDuration {
			// Invalid or expired session
			c.SetCookie(sessionCookieName, "", -1, "/", "", true, true)
			c.Redirect(http.StatusFound, "/admin/login")
			c.Abort()
			return
		}

		c.Next()
	}
}

func createSession() (string, error) {
	token := make([]byte, 32)
	if _, err := rand.Read(token); err != nil {
		return "", err
	}

	sessionToken := base64.URLEncoding.EncodeToString(token)

	sessionsMu.Lock()
	sessions[sessionToken] = &session{
		token:     sessionToken,
		createdAt: time.Now(),
	}
	sessionsMu.Unlock()

	return sessionToken, nil
}

func destroySession(token string) {
	sessionsMu.Lock()
	delete(sessions, token)
	sessionsMu.Unlock()
}

func hashPassword(password string) (string, error) {
	hash, err := bcrypt.GenerateFromPassword([]byte(password), bcrypt.DefaultCost)
	if err != nil {
		return "", err
	}
	return string(hash), nil
}

func verifyPassword(hash, password string) bool {
	err := bcrypt.CompareHashAndPassword([]byte(hash), []byte(password))
	return err == nil
}

// setSecureCookie sets a session cookie with proper security flags
func setSecureCookie(c *gin.Context, token string) {
	isSecure := c.Request.TLS != nil || c.GetHeader("X-Forwarded-Proto") == "https"
	c.SetCookie(sessionCookieName, token, int(sessionDuration.Seconds()), "/", "", isSecure, true)
	c.Header("Set-Cookie", c.Writer.Header().Get("Set-Cookie")+"; SameSite=Lax")
}

// checkRateLimit checks if IP has exceeded login attempts
func checkRateLimit(ip string) bool {
	attemptsMu.RLock()
	attempt, exists := loginAttempts[ip]
	attemptsMu.RUnlock()

	if !exists {
		return true
	}

	// Reset if window expired
	if time.Since(attempt.firstTry) > rateLimitWindow {
		attemptsMu.Lock()
		delete(loginAttempts, ip)
		attemptsMu.Unlock()
		return true
	}

	return attempt.count < maxLoginAttempts
}

// recordFailedAttempt increments failed login counter
func recordFailedAttempt(ip string) {
	attemptsMu.Lock()
	defer attemptsMu.Unlock()

	if attempt, exists := loginAttempts[ip]; exists {
		attempt.count++
	} else {
		loginAttempts[ip] = &loginAttempt{
			count:    1,
			firstTry: time.Now(),
		}
	}
}

// remove rate limit for IP after successful login
func clearFailedAttempts(ip string) {
	attemptsMu.Lock()
	delete(loginAttempts, ip)
	attemptsMu.Unlock()
}

// remove old sessions
func cleanupExpiredSessions() {
	sessionsMu.Lock()
	defer sessionsMu.Unlock()

	for token, sess := range sessions {
		if time.Since(sess.createdAt) > sessionDuration {
			delete(sessions, token)
		}
	}
}

// remove old rate limit entries
func cleanupExpiredAttempts() {
	attemptsMu.Lock()
	defer attemptsMu.Unlock()

	for ip, attempt := range loginAttempts {
		if time.Since(attempt.firstTry) > rateLimitWindow {
			delete(loginAttempts, ip)
		}
	}
}

// create a one-time setup token for first-run
func GenerateSetupToken() string {
	setupTokenMu.Lock()
	defer setupTokenMu.Unlock()

	if setupToken != "" {
		return setupToken
	}

	randomBytes := make([]byte, 8)
	if _, err := rand.Read(randomBytes); err != nil {
		panic(err)
	}

	// Generate a short readable token
	const charset = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"
	for i := range randomBytes {
		randomBytes[i] = charset[randomBytes[i]%byte(len(charset))]
	}

	setupToken = string(randomBytes)
	return setupToken
}

// check if the provided token matches and clear it
func ValidateSetupToken(token string) bool {
	setupTokenMu.Lock()
	defer setupTokenMu.Unlock()

	if setupToken == "" || token != setupToken {
		return false
	}

	// Clear token after successful use
	setupToken = ""
	return true
}
