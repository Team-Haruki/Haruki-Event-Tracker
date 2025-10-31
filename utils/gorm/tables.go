package gorm

import (
	"fmt"
	"sync"
)

// EventTable represents the base event ranking table structure
type EventTable struct {
	Timestamp int64  `gorm:"primaryKey;column:timestamp"`
	UserID    string `gorm:"primaryKey;column:user_id;type:varchar(30)"`
	Score     int    `gorm:"column:score;not null"`
	Rank      int    `gorm:"column:rank;not null"`
}

// WorldBloomTable represents the world bloom ranking table structure
type WorldBloomTable struct {
	Timestamp   int64  `gorm:"primaryKey;column:timestamp"`
	UserID      string `gorm:"primaryKey;column:user_id;type:varchar(30)"`
	CharacterID int    `gorm:"primaryKey;column:character_id"`
	Score       int    `gorm:"column:score;not null"`
	Rank        int    `gorm:"column:rank;not null"`
}

// EventNamesTable represents the event user names table structure
type EventNamesTable struct {
	UserID         string `gorm:"primaryKey;column:user_id;type:varchar(30)"`
	Name           string `gorm:"column:name;type:varchar(300);not null"`
	CheerfulTeamID *int   `gorm:"column:cheerful_team_id"`
}

var (
	eventTableCache      = make(map[int]*DynamicEventTable)
	worldBloomTableCache = make(map[int]*DynamicWorldBloomTable)
	eventNamesTableCache = make(map[int]*DynamicEventNamesTable)
	tableCacheMutex      sync.RWMutex
)

// DynamicEventTable wraps EventTable with dynamic table name
type DynamicEventTable struct {
	EventTable
	tableName string
}

// TableName returns the dynamic table name for GORM
func (t *DynamicEventTable) TableName() string {
	return t.tableName
}

// DynamicWorldBloomTable wraps WorldBloomTable with dynamic table name
type DynamicWorldBloomTable struct {
	WorldBloomTable
	tableName string
}

// TableName returns the dynamic table name for GORM
func (t *DynamicWorldBloomTable) TableName() string {
	return t.tableName
}

// DynamicEventNamesTable wraps EventNamesTable with dynamic table name
type DynamicEventNamesTable struct {
	EventNamesTable
	tableName string
}

// TableName returns the dynamic table name for GORM
func (t *DynamicEventNamesTable) TableName() string {
	return t.tableName
}

// GetEventTableModel returns a model instance for the event table
func GetEventTableModel(eventID int) *DynamicEventTable {
	tableCacheMutex.RLock()
	if table, exists := eventTableCache[eventID]; exists {
		tableCacheMutex.RUnlock()
		return table
	}
	tableCacheMutex.RUnlock()

	tableCacheMutex.Lock()
	defer tableCacheMutex.Unlock()

	// Double-check after acquiring write lock
	if table, exists := eventTableCache[eventID]; exists {
		return table
	}

	table := &DynamicEventTable{
		tableName: fmt.Sprintf("event_%d", eventID),
	}
	eventTableCache[eventID] = table
	return table
}

// GetWorldBloomTableModel returns a model instance for the world bloom table
func GetWorldBloomTableModel(eventID int) *DynamicWorldBloomTable {
	tableCacheMutex.RLock()
	if table, exists := worldBloomTableCache[eventID]; exists {
		tableCacheMutex.RUnlock()
		return table
	}
	tableCacheMutex.RUnlock()

	tableCacheMutex.Lock()
	defer tableCacheMutex.Unlock()

	// Double-check after acquiring write lock
	if table, exists := worldBloomTableCache[eventID]; exists {
		return table
	}

	table := &DynamicWorldBloomTable{
		tableName: fmt.Sprintf("wl_%d", eventID),
	}
	worldBloomTableCache[eventID] = table
	return table
}

// GetEventNamesTableModel returns a model instance for the event names table
func GetEventNamesTableModel(eventID int) *DynamicEventNamesTable {
	tableCacheMutex.RLock()
	if table, exists := eventNamesTableCache[eventID]; exists {
		tableCacheMutex.RUnlock()
		return table
	}
	tableCacheMutex.RUnlock()

	tableCacheMutex.Lock()
	defer tableCacheMutex.Unlock()

	// Double-check after acquiring write lock
	if table, exists := eventNamesTableCache[eventID]; exists {
		return table
	}

	table := &DynamicEventNamesTable{
		tableName: fmt.Sprintf("event_%d_names", eventID),
	}
	eventNamesTableCache[eventID] = table
	return table
}

// ClearTableCache clears all cached table models (useful for testing)
func ClearTableCache() {
	tableCacheMutex.Lock()
	defer tableCacheMutex.Unlock()

	eventTableCache = make(map[int]*DynamicEventTable)
	worldBloomTableCache = make(map[int]*DynamicWorldBloomTable)
	eventNamesTableCache = make(map[int]*DynamicEventNamesTable)
}
