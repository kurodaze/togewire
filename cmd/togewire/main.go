package main

import (
	"flag"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/kurodaze/togewire/internal/config"
	"github.com/kurodaze/togewire/internal/server"
)

func main() {
	// Configure logging with millisecond precision
	log.SetFlags(log.Ldate | log.Ltime | log.Lmicroseconds)
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
	log.Println(banner)

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

	// Generate and display one-time setup token if no password set
	if !cfg.HasAdminPassword() {
		token := server.GenerateSetupToken()
		log.Println("")
		log.Printf("   One-Time Setup Token: %s", token)
		log.Println("")
		log.Println("   This token is required to set your admin password.")
		log.Println("")
	}

	// Create and configure server
	srv := server.New()

	// Handle graceful shutdown
	go func() {
		sigChan := make(chan os.Signal, 1)
		signal.Notify(sigChan, syscall.SIGINT, syscall.SIGTERM)
		<-sigChan

		log.Println("Shutdown signal received...")
		srv.Shutdown()
		os.Exit(0)
	}()

	// Start server
	if err := srv.Run(); err != nil {
		log.Fatalf("Server failed to start: %v", err)
	}
}
