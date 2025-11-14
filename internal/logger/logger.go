package logger

import (
	"fmt"
	"log"
	"os"
	"sync"
)

type Level int

const (
	DEBUG Level = iota
	INFO
	WARN
	ERROR
)

var (
	currentLevel Level
	mu           sync.RWMutex
)

func init() {
	// Default to INFO level
	currentLevel = INFO
	log.SetFlags(log.Ldate | log.Ltime | log.Lmicroseconds)
}

// SetLevel sets the current log level
func SetLevel(level string) {
	mu.Lock()
	defer mu.Unlock()

	switch level {
	case "debug":
		currentLevel = DEBUG
	case "info":
		currentLevel = INFO
	case "warn":
		currentLevel = WARN
	case "error":
		currentLevel = ERROR
	default:
		currentLevel = INFO
	}
}

// Debug logs debug messages (verbose details)
func Debug(format string, v ...interface{}) {
	mu.RLock()
	defer mu.RUnlock()
	if currentLevel <= DEBUG {
		_ = log.Output(2, fmt.Sprintf("[DEBUG] "+format, v...))
	}
}

// Info logs informational messages (important events)
func Info(format string, v ...interface{}) {
	mu.RLock()
	defer mu.RUnlock()
	if currentLevel <= INFO {
		_ = log.Output(2, fmt.Sprintf(format, v...))
	}
}

// Warn logs warning messages
func Warn(format string, v ...interface{}) {
	mu.RLock()
	defer mu.RUnlock()
	if currentLevel <= WARN {
		_ = log.Output(2, fmt.Sprintf("[WARN] "+format, v...))
	}
}

// Error logs error messages
func Error(format string, v ...interface{}) {
	mu.RLock()
	defer mu.RUnlock()
	if currentLevel <= ERROR {
		_ = log.Output(2, fmt.Sprintf("[ERROR] "+format, v...))
	}
}

// Fatal logs a fatal error and exits
func Fatal(format string, v ...interface{}) {
	_ = log.Output(2, fmt.Sprintf("[FATAL] "+format, v...))
	os.Exit(1)
}

// Print logs without level prefix (for banner, etc)
func Print(v ...interface{}) {
	_ = log.Output(2, fmt.Sprint(v...))
}

// Println logs without level prefix with newline
func Println(v ...interface{}) {
	_ = log.Output(2, fmt.Sprintln(v...))
}
