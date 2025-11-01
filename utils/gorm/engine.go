package gorm

import (
	"context"
	"fmt"
	"haruki-tracker/utils/model"
	"time"

	"gorm.io/driver/mysql"
	"gorm.io/driver/postgres"
	"gorm.io/driver/sqlite"
	"gorm.io/gorm"
	"gorm.io/gorm/logger"
)

type DatabaseEngine struct {
	db *gorm.DB
}

func NewDatabaseEngine(dsn string, debug bool) (*DatabaseEngine, error) {
	var dialector gorm.Dialector
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
	sqlDB, err := db.DB()
	if err != nil {
		return nil, fmt.Errorf("failed to get underlying DB: %w", err)
	}
	sqlDB.SetMaxIdleConns(10)
	sqlDB.SetMaxOpenConns(100)
	sqlDB.SetConnMaxLifetime(time.Hour)
	return &DatabaseEngine{db: db}, nil
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
