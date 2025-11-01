package gorm

import (
	"context"
	"fmt"
	"log"
	"os"
	"strings"
	"time"

	"haruki-tracker/utils/model"

	"gorm.io/driver/mysql"
	"gorm.io/driver/postgres"
	"gorm.io/driver/sqlite"
	"gorm.io/gorm"
	"gorm.io/gorm/logger"
	"gorm.io/gorm/schema"
)

type DatabaseEngine struct {
	db *gorm.DB
}

func NewDatabaseEngine(cfg model.GormConfig) (*DatabaseEngine, error) {
	if !cfg.Enabled {
		return nil, fmt.Errorf("database is disabled in configuration")
	}

	var dialector gorm.Dialector
	switch strings.ToLower(cfg.Dialect) {
	case "mysql":
		dialector = mysql.Open(cfg.DSN)
	case "postgres", "postgresql":
		dialector = postgres.Open(cfg.DSN)
	case "sqlite":
		dialector = sqlite.Open(cfg.DSN)
	default:
		return nil, fmt.Errorf("unsupported database dialect: %s", cfg.Dialect)
	}
	logLevel := parseLogLevel(cfg.Logger.Level)
	slowThreshold := parseDuration(cfg.Logger.SlowThreshold, 200*time.Millisecond)
	loggerConfig := logger.Config{
		SlowThreshold:             slowThreshold,
		LogLevel:                  logLevel,
		IgnoreRecordNotFoundError: cfg.Logger.IgnoreRecordNotFoundError,
		Colorful:                  cfg.Logger.Colorful,
	}
	gormConfig := &gorm.Config{
		Logger:      logger.Default.LogMode(logLevel),
		PrepareStmt: cfg.PrepareStmt,
		NamingStrategy: schema.NamingStrategy{
			TablePrefix:   cfg.Naming.TablePrefix,
			SingularTable: cfg.Naming.SingularTable,
		},
		DisableForeignKeyConstraintWhenMigrating: cfg.DisableForeignKeyConstraintWhenMigrating,
	}
	if cfg.Logger.SlowThreshold != "" || cfg.Logger.IgnoreRecordNotFoundError || cfg.Logger.Colorful {
		gormConfig.Logger = logger.New(
			log.New(os.Stdout, "\r\n", log.LstdFlags),
			loggerConfig,
		)
	}
	db, err := gorm.Open(dialector, gormConfig)
	if err != nil {
		return nil, fmt.Errorf("failed to connect to database: %w", err)
	}
	sqlDB, err := db.DB()
	if err != nil {
		return nil, fmt.Errorf("failed to get underlying DB: %w", err)
	}
	if cfg.MaxIdleConns > 0 {
		sqlDB.SetMaxIdleConns(cfg.MaxIdleConns)
	} else {
		sqlDB.SetMaxIdleConns(10)
	}
	if cfg.MaxOpenConns > 0 {
		sqlDB.SetMaxOpenConns(cfg.MaxOpenConns)
	} else {
		sqlDB.SetMaxOpenConns(100)
	}
	if cfg.ConnMaxLifetime != "" {
		lifetime := parseDuration(cfg.ConnMaxLifetime, time.Hour)
		sqlDB.SetConnMaxLifetime(lifetime)
	} else {
		sqlDB.SetConnMaxLifetime(time.Hour)
	}
	return &DatabaseEngine{db: db}, nil
}

func parseLogLevel(level string) logger.LogLevel {
	switch strings.ToLower(level) {
	case "silent":
		return logger.Silent
	case "error":
		return logger.Error
	case "warn", "warning":
		return logger.Warn
	case "info":
		return logger.Info
	default:
		return logger.Warn
	}
}

func parseDuration(durationStr string, defaultDuration time.Duration) time.Duration {
	if durationStr == "" {
		return defaultDuration
	}
	duration, err := time.ParseDuration(durationStr)
	if err != nil {
		return defaultDuration
	}
	return duration
}

func (e *DatabaseEngine) CreateTables(ctx context.Context, models ...interface{}) error {
	for _, tbl := range models {
		if tbl == nil {
			continue
		}
		if err := e.db.WithContext(ctx).AutoMigrate(tbl); err != nil {
			return fmt.Errorf("failed to create table for %T: %w", tbl, err)
		}
	}
	return nil
}

func (e *DatabaseEngine) CreateEventTables(ctx context.Context, server model.SekaiServerRegion, eventID int) error {
	timeIDTable := GetTimeIDTableModel(server, eventID)
	eventUsersTable := GetEventUsersTableModel(server, eventID)
	eventTable := GetEventTableModel(server, eventID)
	worldBloomTable := GetWorldBloomTableModel(server, eventID)
	return e.CreateTables(ctx, timeIDTable, eventUsersTable, eventTable, worldBloomTable)
}

func (e *DatabaseEngine) DB() *gorm.DB {
	return e.db
}

func (e *DatabaseEngine) WithContext(ctx context.Context) *gorm.DB {
	return e.db.WithContext(ctx)
}

func (e *DatabaseEngine) Transaction(ctx context.Context, fn func(*gorm.DB) error) error {
	return e.db.WithContext(ctx).Transaction(fn)
}

func (e *DatabaseEngine) Close() error {
	sqlDB, err := e.db.DB()
	if err != nil {
		return err
	}
	return sqlDB.Close()
}

func (e *DatabaseEngine) Ping(ctx context.Context) error {
	sqlDB, err := e.db.DB()
	if err != nil {
		return err
	}
	return sqlDB.PingContext(ctx)
}
