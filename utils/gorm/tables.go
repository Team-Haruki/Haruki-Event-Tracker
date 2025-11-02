package gorm

import (
	"fmt"
	"sync"

	"haruki-tracker/utils/model"
)

// TimeIDTable maps timestamps to integer IDs to reduce storage
type TimeIDTable struct {
	TimeID    int   `gorm:"primaryKey;autoIncrement;column:time_id"`
	Timestamp int64 `gorm:"uniqueIndex;column:timestamp;not null"`
}

// EventUsersTable stores user information and maps user IDs to integer keys
type EventUsersTable struct {
	UserIDKey      int    `gorm:"primaryKey;autoIncrement;column:user_id_key"`
	UserID         string `gorm:"uniqueIndex;column:user_id;type:varchar(30);not null"`
	Name           string `gorm:"column:name;type:varchar(300);not null"`
	CheerfulTeamID *int   `gorm:"column:cheerful_team_id"`
}

// EventTable stores event ranking data
// Foreign key relationships (enforced at application level for performance):
//   - TimeID references TimeIDTable.TimeID
//   - UserIDKey references EventUsersTable.UserIDKey
type EventTable struct {
	TimeID    int `gorm:"primaryKey;column:time_id"`
	UserIDKey int `gorm:"primaryKey;column:user_id_key"`
	Score     int `gorm:"column:score;not null"`
	Rank      int `gorm:"column:rank;not null"`
}

// WorldBloomTable stores world bloom ranking data
// Foreign key relationships (enforced at application level for performance):
//   - TimeID references TimeIDTable.TimeID
//   - UserIDKey references EventUsersTable.UserIDKey
type WorldBloomTable struct {
	TimeID      int `gorm:"primaryKey;column:time_id"`
	UserIDKey   int `gorm:"primaryKey;column:user_id_key"`
	CharacterID int `gorm:"primaryKey;column:character_id"`
	Score       int `gorm:"column:score;not null"`
	Rank        int `gorm:"column:rank;not null"`
}

type serverTableCache struct {
	timeIDTableCache     map[int]*DynamicTimeIDTable
	eventUsersTableCache map[int]*DynamicEventUsersTable
	eventTableCache      map[int]*DynamicEventTable
	worldBloomTableCache map[int]*DynamicWorldBloomTable
	mu                   sync.RWMutex
}

var (
	serverCaches      = make(map[model.SekaiServerRegion]*serverTableCache)
	serverCachesMutex sync.RWMutex
)

func getOrCreateServerCache(server model.SekaiServerRegion) *serverTableCache {
	serverCachesMutex.RLock()
	if cache, exists := serverCaches[server]; exists {
		serverCachesMutex.RUnlock()
		return cache
	}
	serverCachesMutex.RUnlock()
	serverCachesMutex.Lock()
	defer serverCachesMutex.Unlock()
	if cache, exists := serverCaches[server]; exists {
		return cache
	}
	cache := &serverTableCache{
		timeIDTableCache:     make(map[int]*DynamicTimeIDTable),
		eventUsersTableCache: make(map[int]*DynamicEventUsersTable),
		eventTableCache:      make(map[int]*DynamicEventTable),
		worldBloomTableCache: make(map[int]*DynamicWorldBloomTable),
	}
	serverCaches[server] = cache
	return cache
}

type DynamicTimeIDTable struct {
	TimeIDTable
	tableName string
}

func (t *DynamicTimeIDTable) TableName() string {
	return t.tableName
}

type DynamicEventUsersTable struct {
	EventUsersTable
	tableName string
}

func (t *DynamicEventUsersTable) TableName() string {
	return t.tableName
}

type DynamicEventTable struct {
	EventTable
	tableName string
}

func (t *DynamicEventTable) TableName() string {
	return t.tableName
}

type DynamicWorldBloomTable struct {
	WorldBloomTable
	tableName string
}

func (t *DynamicWorldBloomTable) TableName() string {
	return t.tableName
}

func GetTimeIDTableModel(server model.SekaiServerRegion, eventID int) *DynamicTimeIDTable {
	cache := getOrCreateServerCache(server)
	cache.mu.RLock()
	if table, exists := cache.timeIDTableCache[eventID]; exists {
		cache.mu.RUnlock()
		return table
	}
	cache.mu.RUnlock()
	cache.mu.Lock()
	defer cache.mu.Unlock()
	if table, exists := cache.timeIDTableCache[eventID]; exists {
		return table
	}
	table := &DynamicTimeIDTable{
		tableName: fmt.Sprintf("event_%d_time_id", eventID),
	}
	cache.timeIDTableCache[eventID] = table
	return table
}

func GetEventUsersTableModel(server model.SekaiServerRegion, eventID int) *DynamicEventUsersTable {
	cache := getOrCreateServerCache(server)
	cache.mu.RLock()
	if table, exists := cache.eventUsersTableCache[eventID]; exists {
		cache.mu.RUnlock()
		return table
	}
	cache.mu.RUnlock()
	cache.mu.Lock()
	defer cache.mu.Unlock()
	if table, exists := cache.eventUsersTableCache[eventID]; exists {
		return table
	}
	table := &DynamicEventUsersTable{
		tableName: fmt.Sprintf("event_%d_users", eventID),
	}
	cache.eventUsersTableCache[eventID] = table
	return table
}

func GetEventTableModel(server model.SekaiServerRegion, eventID int) *DynamicEventTable {
	cache := getOrCreateServerCache(server)
	cache.mu.RLock()
	if table, exists := cache.eventTableCache[eventID]; exists {
		cache.mu.RUnlock()
		return table
	}
	cache.mu.RUnlock()
	cache.mu.Lock()
	defer cache.mu.Unlock()
	if table, exists := cache.eventTableCache[eventID]; exists {
		return table
	}
	table := &DynamicEventTable{
		tableName: fmt.Sprintf("event_%d", eventID),
	}
	cache.eventTableCache[eventID] = table
	return table
}

func GetWorldBloomTableModel(server model.SekaiServerRegion, eventID int) *DynamicWorldBloomTable {
	cache := getOrCreateServerCache(server)
	cache.mu.RLock()
	if table, exists := cache.worldBloomTableCache[eventID]; exists {
		cache.mu.RUnlock()
		return table
	}
	cache.mu.RUnlock()
	cache.mu.Lock()
	defer cache.mu.Unlock()
	if table, exists := cache.worldBloomTableCache[eventID]; exists {
		return table
	}
	table := &DynamicWorldBloomTable{
		tableName: fmt.Sprintf("wl_%d", eventID),
	}
	cache.worldBloomTableCache[eventID] = table
	return table
}

func ClearTableCache() {
	serverCachesMutex.Lock()
	defer serverCachesMutex.Unlock()
	serverCaches = make(map[model.SekaiServerRegion]*serverTableCache)
}

func ClearServerTableCache(server model.SekaiServerRegion) {
	serverCachesMutex.Lock()
	defer serverCachesMutex.Unlock()
	delete(serverCaches, server)
}
