package gorm

import (
	"context"
	"fmt"
	"time"

	"gorm.io/driver/mysql"
	"gorm.io/driver/postgres"
	"gorm.io/driver/sqlite"
	"gorm.io/gorm"
	"gorm.io/gorm/logger"
)

// DatabaseEngine wraps GORM database connection and operations
type DatabaseEngine struct {
	db *gorm.DB
}

// NewDatabaseEngine creates a new database engine with the given DSN
// Supports: mysql://, postgres://, sqlite://
func NewDatabaseEngine(dsn string, debug bool) (*DatabaseEngine, error) {
	var dialector gorm.Dialector

	// Parse DSN to determine database type
	// Simple parsing - you may want to use a proper URL parser
	switch {
	case len(dsn) > 8 && dsn[:8] == "mysql://":
		dialector = mysql.Open(dsn[8:])
	case len(dsn) > 11 && dsn[:11] == "postgres://":
		dialector = postgres.Open(dsn)
	case len(dsn) > 9 && dsn[:9] == "sqlite://":
		dialector = sqlite.Open(dsn[9:])
	default:
		// Default to MySQL for backward compatibility
		dialector = mysql.Open(dsn)
	}

	config := &gorm.Config{}
	if debug {
		config.Logger = logger.Default.LogMode(logger.Info)
	} else {
		config.Logger = logger.Default.LogMode(logger.Silent)
	}

	db, err := gorm.Open(dialector, config)
	if err != nil {
		return nil, fmt.Errorf("failed to connect to database: %w", err)
	}

	// Set connection pool settings
	sqlDB, err := db.DB()
	if err != nil {
		return nil, fmt.Errorf("failed to get underlying DB: %w", err)
	}

	sqlDB.SetMaxIdleConns(10)
	sqlDB.SetMaxOpenConns(100)
	sqlDB.SetConnMaxLifetime(time.Hour)

	return &DatabaseEngine{db: db}, nil
}

// InitEngine initializes the database and creates base tables if needed
func (e *DatabaseEngine) InitEngine(ctx context.Context) error {
	// You can add base table initialization here if needed
	return nil
}

// CreateTables creates tables dynamically for the given table models
func (e *DatabaseEngine) CreateTables(ctx context.Context, models ...interface{}) error {
	for _, model := range models {
		if model == nil {
			continue
		}
		if err := e.db.WithContext(ctx).AutoMigrate(model); err != nil {
			return fmt.Errorf("failed to create table for %T: %w", model, err)
		}
	}
	return nil
}

// CreateEventTables creates all three tables for a specific event
func (e *DatabaseEngine) CreateEventTables(ctx context.Context, eventID int) error {
	eventTable := GetEventTableModel(eventID)
	worldBloomTable := GetWorldBloomTableModel(eventID)
	namesTable := GetEventNamesTableModel(eventID)

	return e.CreateTables(ctx, eventTable, worldBloomTable, namesTable)
}

// DB returns the underlying GORM database instance
func (e *DatabaseEngine) DB() *gorm.DB {
	return e.db
}

// WithContext returns a new GORM DB instance with the given context
func (e *DatabaseEngine) WithContext(ctx context.Context) *gorm.DB {
	return e.db.WithContext(ctx)
}

// Transaction executes a function within a database transaction
func (e *DatabaseEngine) Transaction(ctx context.Context, fn func(*gorm.DB) error) error {
	return e.db.WithContext(ctx).Transaction(fn)
}

// Close closes the database connection
func (e *DatabaseEngine) Close() error {
	sqlDB, err := e.db.DB()
	if err != nil {
		return err
	}
	return sqlDB.Close()
}

// Ping checks if the database connection is alive
func (e *DatabaseEngine) Ping(ctx context.Context) error {
	sqlDB, err := e.db.DB()
	if err != nil {
		return err
	}
	return sqlDB.PingContext(ctx)
}
