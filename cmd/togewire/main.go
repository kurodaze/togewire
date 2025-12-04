package main

import (
	"flag"
	"os"
	"os/signal"
	"syscall"

	"github.com/kurodaze/togewire"
	"github.com/kurodaze/togewire/internal/config"
	"github.com/kurodaze/togewire/internal/logger"
	"github.com/kurodaze/togewire/internal/server"
)

func init() {
	if os.Getenv("GOMEMLIMIT") == "" {
		os.Setenv("GOMEMLIMIT", "40MiB")
	}
}

func main() {
	banner := `
 ███████████    ███████      █████████  ██████████ █████   ███   █████ █████ ███████████   ██████████
░█░░░███░░░█  ███░░░░░███   ███░░░░░███░░███░░░░░█░░███   ░███  ░░███ ░░███ ░░███░░░░░███ ░░███░░░░░█
░   ░███  ░  ███     ░░███ ███     ░░░  ░███  █ ░  ░███   ░███   ░███  ░███  ░███    ░███  ░███  █ ░ 
    ░███    ░███      ░███░███          ░██████    ░███   ░███   ░███  ░███  ░██████████   ░██████   
    ░███    ░███      ░███░███    █████ ░███░░█    ░░███  █████  ███   ░███  ░███░░░░░███  ░███░░█   
    ░███    ░░███     ███ ░░███  ░░███  ░███ ░   █  ░░░█████░█████░    ░███  ░███    ░███  ░███ ░   █
    █████    ░░░███████░   ░░█████████  ██████████    ░░███ ░░███      █████ █████   █████ ██████████
   ░░░░░       ░░░░░░░      ░░░░░░░░░  ░░░░░░░░░░      ░░░   ░░░      ░░░░░ ░░░░░   ░░░░░ ░░░░░░░░░░ 
`
	logger.Println(banner)

	// Parse command line flags
	var (
		port = flag.Int("port", 7093, "Server port")
		host = flag.String("host", "0.0.0.0", "Server host")
	)
	flag.Parse()

	// Load configuration
	cfg := config.Get()
	cfg.Port = *port
	cfg.Host = *host

	// Initialize logger with configured level
	logger.SetLevel(cfg.LogLevel)

	// Generate and display one-time setup token if no password set
	if !cfg.HasAdminPassword() {
		token := server.GenerateSetupToken()
		logger.Println("")
		logger.Info("   One-Time Setup Token: %s", token)
		logger.Println("")
		logger.Println("   This token is required to set your admin password.")
		logger.Println("")
	}

	// Set embedded files for the server
	server.SetEmbeddedFiles(togewire.EmbeddedFiles)

	// Create and configure server
	srv := server.New()

	// Handle graceful shutdown
	go func() {
		sigChan := make(chan os.Signal, 1)
		signal.Notify(sigChan, syscall.SIGINT, syscall.SIGTERM)
		<-sigChan

		logger.Info("Shutdown signal received...")
		srv.Shutdown()
		os.Exit(0)
	}()

	// Start server
	if err := srv.Run(); err != nil {
		logger.Fatal("Server failed to start: %v", err)
	}
}
