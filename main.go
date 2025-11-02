package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/signal"
	"syscall"
	"time"

	"haruki-tracker/api"
	"haruki-tracker/config"
	harukiLogger "haruki-tracker/utils/logger"

	"github.com/gofiber/fiber/v2"
	"github.com/gofiber/fiber/v2/middleware/logger"
	"github.com/gofiber/fiber/v2/middleware/recover"
)

func main() {
	var logFile *os.File
	var loggerWriter io.Writer = os.Stdout
	if config.Cfg.Backend.MainLogFile != "" {
		var err error
		logFile, err = os.OpenFile(config.Cfg.Backend.MainLogFile, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0644)
		if err != nil {
			mainLogger := harukiLogger.NewLogger("Main", config.Cfg.Backend.LogLevel, os.Stdout)
			mainLogger.Errorf("failed to open main log file: %v", err)
			os.Exit(1)
		}
		loggerWriter = io.MultiWriter(os.Stdout, logFile)
		defer func(logFile *os.File) {
			_ = logFile.Close()
		}(logFile)
	}

	mainLogger := harukiLogger.NewLogger("Main", config.Cfg.Backend.LogLevel, loggerWriter)
	mainLogger.Infof("========================= Haruki Event Tracker %s =========================", config.Version)
	mainLogger.Infof("Powered By Haruki Dev Team")
	mainLogger.Infof("Initializing API utilities...")
	if err := api.InitAPIUtils(config.Cfg); err != nil {
		mainLogger.Errorf("failed to initialize API utils: %v", err)
		os.Exit(1)
	}
	mainLogger.Infof("API utilities initialized successfully")
	app := fiber.New(fiber.Config{
		BodyLimit:             30 * 1024 * 1024, // 30MB
		DisableStartupMessage: false,
		ErrorHandler: func(c *fiber.Ctx, err error) error {
			code := fiber.StatusInternalServerError
			var e *fiber.Error
			if errors.As(err, &e) {
				code = e.Code
			}
			return c.Status(code).JSON(fiber.Map{
				"error": err.Error(),
			})
		},
	})
	app.Use(recover.New())
	if config.Cfg.Backend.AccessLog != "" {
		logCfg := logger.Config{Format: config.Cfg.Backend.AccessLog}
		if config.Cfg.Backend.AccessLogPath != "" {
			accessLogFile, err := os.OpenFile(config.Cfg.Backend.AccessLogPath, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0644)
			if err != nil {
				mainLogger.Errorf("failed to open access log file: %v", err)
				os.Exit(1)
			}
			defer func(accessLogFile *os.File) {
				_ = accessLogFile.Close()
			}(accessLogFile)
			logCfg.Output = accessLogFile
		}
		app.Use(logger.New(logCfg))
	}
	api.RegisterRoutes(app)
	mainLogger.Infof("API routes registered")
	sigChan := make(chan os.Signal, 1)
	signal.Notify(sigChan, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		addr := fmt.Sprintf("%s:%d", config.Cfg.Backend.Host, config.Cfg.Backend.Port)
		mainLogger.Infof("Starting server on %s...", addr)
		var err error
		if config.Cfg.Backend.SSL {
			mainLogger.Infof("SSL enabled, using certificate: %s", config.Cfg.Backend.SSLCert)
			err = app.ListenTLS(addr, config.Cfg.Backend.SSLCert, config.Cfg.Backend.SSLKey)
		} else {
			mainLogger.Infof("SSL disabled, starting HTTP server")
			err = app.Listen(addr)
		}
		if err != nil {
			mainLogger.Errorf("failed to start server: %v", err)
			os.Exit(1)
		}
	}()
	mainLogger.Infof("Server started successfully")
	mainLogger.Infof("Press Ctrl+C to shutdown")
	<-sigChan
	mainLogger.Infof("Shutdown signal received, gracefully shutting down...")
	if err := app.ShutdownWithTimeout(10 * time.Second); err != nil {
		mainLogger.Errorf("Server shutdown error: %v", err)
	}
	if err := api.Shutdown(); err != nil {
		mainLogger.Errorf("API shutdown error: %v", err)
	}
	mainLogger.Infof("Server stopped gracefully")
}
