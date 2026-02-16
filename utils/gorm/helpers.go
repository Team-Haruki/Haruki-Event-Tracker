package gorm

import (
	"context"
	"errors"
	"fmt"
	"sync"

	"haruki-tracker/utils/model"

	"gorm.io/gorm"
)

func GetUserData(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, userID string) (*model.RecordedUserNameSchema, error) {
	table := GetEventUsersTableModel(server, eventID)
	var result EventUsersTable
	err := engine.WithContext(ctx).
		Table(table.TableName()).
		Where("user_id = ?", userID).
		First(&result).Error

	if err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch user data: %w", err)
	}
	return &model.RecordedUserNameSchema{
		UserID:         result.UserID,
		Name:           result.Name,
		CheerfulTeamID: result.CheerfulTeamID,
	}, nil
}

func FetchLatestRanking(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, userID string) (*model.RecordedRankingSchema, error) {
	eventTable := GetEventTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var result model.RecordedRankingSchema
	query := fmt.Sprintf(`SELECT t.timestamp, u.user_id, e.score, e.rank 
		FROM %s AS e 
		INNER JOIN %s AS t ON e.time_id = t.time_id 
		INNER JOIN %s AS u ON e.user_id_key = u.user_id_key 
		WHERE u.user_id = ? 
		ORDER BY t.timestamp DESC 
		LIMIT 1`,
		engine.db.Statement.Quote(eventTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()),
		engine.db.Statement.Quote(usersTable.TableName()))
	err := engine.WithContext(ctx).
		Raw(query, userID).
		Scan(&result).Error
	if err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch latest ranking: %w", err)
	}
	if result.UserID == "" {
		return nil, nil
	}
	return &result, nil
}

func FetchAllRankings(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, userID string) ([]*model.RecordedRankingSchema, error) {
	eventTable := GetEventTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var results []*model.RecordedRankingSchema
	query := fmt.Sprintf(`SELECT t.timestamp, u.user_id, e.score, e.rank 
		FROM %s AS e 
		INNER JOIN %s AS t ON e.time_id = t.time_id 
		INNER JOIN %s AS u ON e.user_id_key = u.user_id_key 
		WHERE u.user_id = ? 
		ORDER BY t.timestamp ASC`,
		engine.db.Statement.Quote(eventTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()),
		engine.db.Statement.Quote(usersTable.TableName()))
	err := engine.WithContext(ctx).
		Raw(query, userID).
		Scan(&results).Error
	if err != nil {
		return nil, fmt.Errorf("failed to fetch all rankings: %w", err)
	}
	return results, nil
}

func FetchLatestWorldBloomRanking(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, userID string, characterID int) (*model.RecordedWorldBloomRankingSchema, error) {
	wlTable := GetWorldBloomTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var result model.RecordedWorldBloomRankingSchema
	query := fmt.Sprintf(`SELECT t.timestamp, u.user_id, w.score, w.rank, w.character_id 
		FROM %s AS w 
		INNER JOIN %s AS t ON w.time_id = t.time_id 
		INNER JOIN %s AS u ON w.user_id_key = u.user_id_key 
		WHERE u.user_id = ? AND w.character_id = ? 
		ORDER BY t.timestamp DESC 
		LIMIT 1`,
		engine.db.Statement.Quote(wlTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()),
		engine.db.Statement.Quote(usersTable.TableName()))
	err := engine.WithContext(ctx).
		Raw(query, userID, characterID).
		Scan(&result).Error
	if err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch latest world bloom ranking: %w", err)
	}
	if result.UserID == "" {
		return nil, nil
	}
	return &result, nil
}

func FetchAllWorldBloomRankings(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, userID string, characterID int) ([]*model.RecordedWorldBloomRankingSchema, error) {
	wlTable := GetWorldBloomTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var results []*model.RecordedWorldBloomRankingSchema
	query := fmt.Sprintf(`SELECT t.timestamp, u.user_id, w.score, w.rank, w.character_id 
		FROM %s AS w 
		INNER JOIN %s AS t ON w.time_id = t.time_id 
		INNER JOIN %s AS u ON w.user_id_key = u.user_id_key 
		WHERE u.user_id = ? AND w.character_id = ? 
		ORDER BY t.timestamp ASC`,
		engine.db.Statement.Quote(wlTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()),
		engine.db.Statement.Quote(usersTable.TableName()))
	err := engine.WithContext(ctx).
		Raw(query, userID, characterID).
		Scan(&results).Error
	if err != nil {
		return nil, fmt.Errorf("failed to fetch all world bloom rankings: %w", err)
	}
	return results, nil
}

func FetchLatestRankingByRank(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, rank int) (*model.RecordedRankingSchema, error) {
	eventTable := GetEventTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var result model.RecordedRankingSchema
	query := fmt.Sprintf(`SELECT t.timestamp, u.user_id, e.score, e.rank 
		FROM %s AS e 
		INNER JOIN %s AS t ON e.time_id = t.time_id 
		INNER JOIN %s AS u ON e.user_id_key = u.user_id_key 
		WHERE e.rank = ? 
		ORDER BY t.timestamp DESC 
		LIMIT 1`,
		engine.db.Statement.Quote(eventTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()),
		engine.db.Statement.Quote(usersTable.TableName()))
	err := engine.WithContext(ctx).
		Raw(query, rank).
		Scan(&result).Error
	if err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch latest ranking by rank: %w", err)
	}
	if result.UserID == "" {
		return nil, nil
	}
	return &result, nil
}

func FetchAllRankingsByRank(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, rank int) ([]*model.RecordedRankingSchema, error) {
	eventTable := GetEventTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var results []*model.RecordedRankingSchema
	query := fmt.Sprintf(`SELECT t.timestamp, u.user_id, e.score, e.rank 
		FROM %s AS e 
		INNER JOIN %s AS t ON e.time_id = t.time_id 
		INNER JOIN %s AS u ON e.user_id_key = u.user_id_key 
		WHERE e.rank = ? 
		ORDER BY t.timestamp ASC`,
		engine.db.Statement.Quote(eventTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()),
		engine.db.Statement.Quote(usersTable.TableName()))
	err := engine.WithContext(ctx).
		Raw(query, rank).
		Scan(&results).Error
	if err != nil {
		return nil, fmt.Errorf("failed to fetch all rankings by rank: %w", err)
	}
	return results, nil
}

func FetchLatestWorldBloomRankingByRank(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, rank int, characterID int) (*model.RecordedWorldBloomRankingSchema, error) {
	wlTable := GetWorldBloomTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var result model.RecordedWorldBloomRankingSchema
	query := fmt.Sprintf(`SELECT t.timestamp, u.user_id, w.score, w.rank, w.character_id 
		FROM %s AS w 
		INNER JOIN %s AS t ON w.time_id = t.time_id 
		INNER JOIN %s AS u ON w.user_id_key = u.user_id_key 
		WHERE w.rank = ? AND w.character_id = ? 
		ORDER BY t.timestamp DESC 
		LIMIT 1`,
		engine.db.Statement.Quote(wlTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()),
		engine.db.Statement.Quote(usersTable.TableName()))
	err := engine.WithContext(ctx).
		Raw(query, rank, characterID).
		Scan(&result).Error
	if err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch latest world bloom ranking by rank: %w", err)
	}
	if result.UserID == "" {
		return nil, nil
	}
	return &result, nil
}

func FetchAllWorldBloomRankingsByRank(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, rank int, characterID int) ([]*model.RecordedWorldBloomRankingSchema, error) {
	wlTable := GetWorldBloomTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var results []*model.RecordedWorldBloomRankingSchema
	query := fmt.Sprintf(`SELECT t.timestamp, u.user_id, w.score, w.rank, w.character_id 
		FROM %s AS w 
		INNER JOIN %s AS t ON w.time_id = t.time_id 
		INNER JOIN %s AS u ON w.user_id_key = u.user_id_key 
		WHERE w.rank = ? AND w.character_id = ? 
		ORDER BY t.timestamp ASC`,
		engine.db.Statement.Quote(wlTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()),
		engine.db.Statement.Quote(usersTable.TableName()))
	err := engine.WithContext(ctx).
		Raw(query, rank, characterID).
		Scan(&results).Error
	if err != nil {
		return nil, fmt.Errorf("failed to fetch all world bloom rankings by rank: %w", err)
	}
	return results, nil
}

func FetchRankingLines(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, ranks []int) ([]*model.RankingLineScoreSchema, error) {
	eventTable := GetEventTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)

	query := fmt.Sprintf(`SELECT t.timestamp, e.score, e.rank 
		FROM %s AS e 
		INNER JOIN %s AS t ON e.time_id = t.time_id 
		WHERE e.rank = ? 
		ORDER BY t.timestamp DESC 
		LIMIT 1`,
		engine.db.Statement.Quote(eventTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()))

	var mu sync.Mutex
	var wg sync.WaitGroup
	result := make([]*model.RankingLineScoreSchema, 0, len(ranks))

	for _, rank := range ranks {
		wg.Add(1)
		go func(r int) {
			defer wg.Done()
			var record struct {
				Timestamp int64
				Score     int
				Rank      int
			}
			err := engine.WithContext(ctx).
				Raw(query, r).
				Scan(&record).Error
			if err == nil && record.Rank > 0 {
				mu.Lock()
				result = append(result, &model.RankingLineScoreSchema{
					Rank:      record.Rank,
					Score:     record.Score,
					Timestamp: record.Timestamp,
				})
				mu.Unlock()
			}
		}(rank)
	}

	wg.Wait()
	return result, nil
}

func FetchRankingScoreGrowths(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, ranks []int, startTime int64) ([]*model.RankingScoreGrowthSchema, error) {
	eventTable := GetEventTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)

	query := fmt.Sprintf(`SELECT t.timestamp, e.score, e.rank 
		FROM %s AS e 
		INNER JOIN %s AS t ON e.time_id = t.time_id 
		WHERE e.rank = ? AND t.timestamp >= ? 
		ORDER BY t.timestamp ASC`,
		engine.db.Statement.Quote(eventTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()))

	var mu sync.Mutex
	var wg sync.WaitGroup
	result := make([]*model.RankingScoreGrowthSchema, 0, len(ranks))

	for _, rank := range ranks {
		wg.Add(1)
		go func(r int) {
			defer wg.Done()
			var records []struct {
				Timestamp int64
				Score     int
				Rank      int
			}
			err := engine.WithContext(ctx).
				Raw(query, r, startTime).
				Scan(&records).Error
			if err == nil && len(records) >= 2 {
				earlier := records[0]
				latest := records[len(records)-1]
				growth := latest.Score - earlier.Score
				diff := latest.Timestamp - earlier.Timestamp
				mu.Lock()
				result = append(result, &model.RankingScoreGrowthSchema{
					Rank:             r,
					TimestampLatest:  latest.Timestamp,
					ScoreLatest:      latest.Score,
					TimestampEarlier: &earlier.Timestamp,
					ScoreEarlier:     &earlier.Score,
					TimeDiff:         &diff,
					Growth:           &growth,
				})
				mu.Unlock()
			}
		}(rank)
	}

	wg.Wait()
	return result, nil
}

func FetchWorldBloomRankingLines(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, characterID int, ranks []int) ([]*model.RankingLineScoreSchema, error) {
	wlTable := GetWorldBloomTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)

	query := fmt.Sprintf(`SELECT t.timestamp, w.score, w.rank 
		FROM %s AS w 
		INNER JOIN %s AS t ON w.time_id = t.time_id 
		WHERE w.rank = ? AND w.character_id = ? 
		ORDER BY t.timestamp DESC 
		LIMIT 1`,
		engine.db.Statement.Quote(wlTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()))

	var mu sync.Mutex
	var wg sync.WaitGroup
	result := make([]*model.RankingLineScoreSchema, 0, len(ranks))

	for _, rank := range ranks {
		wg.Add(1)
		go func(r int) {
			defer wg.Done()
			var record struct {
				Timestamp int64
				Score     int
				Rank      int
			}
			err := engine.WithContext(ctx).
				Raw(query, r, characterID).
				Scan(&record).Error
			if err == nil && record.Rank > 0 {
				mu.Lock()
				result = append(result, &model.RankingLineScoreSchema{
					Rank:      record.Rank,
					Score:     record.Score,
					Timestamp: record.Timestamp,
				})
				mu.Unlock()
			}
		}(rank)
	}

	wg.Wait()
	return result, nil
}

func FetchWorldBloomRankingScoreGrowths(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, characterID int, ranks []int, startTime int64) ([]*model.RankingScoreGrowthSchema, error) {
	wlTable := GetWorldBloomTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)

	query := fmt.Sprintf(`SELECT t.timestamp, w.score, w.rank 
		FROM %s AS w 
		INNER JOIN %s AS t ON w.time_id = t.time_id 
		WHERE w.rank = ? AND w.character_id = ? AND t.timestamp >= ? 
		ORDER BY t.timestamp ASC`,
		engine.db.Statement.Quote(wlTable.TableName()),
		engine.db.Statement.Quote(timeIDTable.TableName()))

	var mu sync.Mutex
	var wg sync.WaitGroup
	result := make([]*model.RankingScoreGrowthSchema, 0, len(ranks))

	for _, rank := range ranks {
		wg.Add(1)
		go func(r int) {
			defer wg.Done()
			var records []struct {
				Timestamp int64
				Score     int
				Rank      int
			}
			err := engine.WithContext(ctx).
				Raw(query, r, characterID, startTime).
				Scan(&records).Error
			if err == nil && len(records) >= 2 {
				earlier := records[0]
				latest := records[len(records)-1]
				growth := latest.Score - earlier.Score
				diff := latest.Timestamp - earlier.Timestamp
				mu.Lock()
				result = append(result, &model.RankingScoreGrowthSchema{
					Rank:             r,
					TimestampLatest:  latest.Timestamp,
					ScoreLatest:      latest.Score,
					TimestampEarlier: &earlier.Timestamp,
					ScoreEarlier:     &earlier.Score,
					TimeDiff:         &diff,
					Growth:           &growth,
				})
				mu.Unlock()
			}
		}(rank)
	}

	wg.Wait()
	return result, nil
}

func batchGetOrCreateTimeIDs(tx *gorm.DB, timeIDTable *DynamicTimeIDTable, timestamps map[int64]bool, status int8) (map[int64]int, error) {
	timeIDLookup := make(map[int64]int)
	for timestamp := range timestamps {
		var result TimeIDTable
		err := tx.Table(timeIDTable.TableName()).
			Where("timestamp = ?", timestamp).
			First(&result).Error
		if errors.Is(err, gorm.ErrRecordNotFound) {
			newRecord := &TimeIDTable{Timestamp: timestamp, Status: status}
			err = tx.Table(timeIDTable.TableName()).Create(newRecord).Error
			if err != nil {
				return nil, fmt.Errorf("failed to create time_id: %w", err)
			}
			timeIDLookup[timestamp] = newRecord.TimeID
		} else if err != nil {
			return nil, fmt.Errorf("failed to query time_id: %w", err)
		} else {
			timeIDLookup[timestamp] = result.TimeID
		}
	}
	return timeIDLookup, nil
}

func WriteHeartbeat(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, timestamp int64, status int8) error {
	return engine.Transaction(ctx, func(tx *gorm.DB) error {
		timeIDTable := GetTimeIDTableModel(server, eventID)
		timestampMap := map[int64]bool{timestamp: true}
		_, err := batchGetOrCreateTimeIDs(tx, timeIDTable, timestampMap, status)
		return err
	})
}

func batchGetOrCreateUserIDKeys(tx *gorm.DB, usersTable *DynamicEventUsersTable, userMap map[string]struct {
	Name           string
	CheerfulTeamID *int
}) (map[string]int, error) {
	userIDKeyLookup := make(map[string]int)
	for userID, userData := range userMap {
		var result EventUsersTable
		err := tx.Table(usersTable.TableName()).
			Where("user_id = ?", userID).
			First(&result).Error
		if errors.Is(err, gorm.ErrRecordNotFound) {
			newRecord := &EventUsersTable{
				UserID:         userID,
				Name:           userData.Name,
				CheerfulTeamID: userData.CheerfulTeamID,
			}
			err = tx.Table(usersTable.TableName()).Create(newRecord).Error
			if err != nil {
				return nil, fmt.Errorf("failed to create user: %w", err)
			}
			userIDKeyLookup[userID] = newRecord.UserIDKey
		} else if err != nil {
			return nil, fmt.Errorf("failed to query user_id_key: %w", err)
		} else {
			if result.Name != userData.Name || (userData.CheerfulTeamID != nil && (result.CheerfulTeamID == nil || *result.CheerfulTeamID != *userData.CheerfulTeamID)) {
				result.Name = userData.Name
				result.CheerfulTeamID = userData.CheerfulTeamID
				err = tx.Table(usersTable.TableName()).Save(&result).Error
				if err != nil {
					return nil, fmt.Errorf("failed to update user: %w", err)
				}
			}
			userIDKeyLookup[userID] = result.UserIDKey
		}
	}
	return userIDKeyLookup, nil
}

// BatchInsertEventRankings inserts event ranking records with deduplication.
// The prevState map is modified by this function to track (score, rank) state.
// Note: Caller must ensure sequential access to prevState (no concurrent calls).
func BatchInsertEventRankings(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, records []*model.PlayerEventRankingRecordSchema, prevState map[int]model.PlayerState) error {
	if len(records) == 0 {
		return nil
	}
	return engine.Transaction(ctx, func(tx *gorm.DB) error {
		timestampMap := make(map[int64]bool)
		userMap := make(map[string]struct {
			Name           string
			CheerfulTeamID *int
		})
		for _, record := range records {
			timestampMap[record.Timestamp] = true
			if _, exists := userMap[record.UserID]; !exists {
				userMap[record.UserID] = struct {
					Name           string
					CheerfulTeamID *int
				}{
					Name:           record.Name,
					CheerfulTeamID: record.CheerfulTeamID,
				}
			}
		}
		timeIDTable := GetTimeIDTableModel(server, eventID)
		timeIDLookup, err := batchGetOrCreateTimeIDs(tx, timeIDTable, timestampMap, 0)
		if err != nil {
			return err
		}
		usersTable := GetEventUsersTableModel(server, eventID)
		userIDKeyLookup, err := batchGetOrCreateUserIDKeys(tx, usersTable, userMap)
		if err != nil {
			return err
		}

		// Filter records: only keep those with changed (score, rank)
		var changedRecords []*model.PlayerEventRankingRecordSchema
		for _, record := range records {
			userIDKey := userIDKeyLookup[record.UserID]
			last, exists := prevState[userIDKey]
			if !exists || last.Score != record.Score || last.Rank != record.Rank {
				changedRecords = append(changedRecords, record)
				prevState[userIDKey] = model.PlayerState{Score: record.Score, Rank: record.Rank}
			}
		}
		
		// If no changes, skip EventTable write
		if len(changedRecords) == 0 {
			return nil
		}
		
		eventTable := GetEventTableModel(server, eventID)
		eventRecords := make([]*EventTable, 0, len(changedRecords))
		for _, record := range changedRecords {
			eventRecords = append(eventRecords, &EventTable{
				TimeID:    timeIDLookup[record.Timestamp],
				UserIDKey: userIDKeyLookup[record.UserID],
				Score:     record.Score,
				Rank:      record.Rank,
			})
		}
		return tx.Table(eventTable.TableName()).Create(eventRecords).Error
	})
}

// BatchInsertWorldBloomRankings inserts world bloom ranking records with deduplication.
// The prevState map is modified by this function to track (score, rank) state per (user, character).
// Note: Caller must ensure sequential access to prevState (no concurrent calls).
func BatchInsertWorldBloomRankings(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, records []*model.PlayerWorldBloomRankingRecordSchema, prevState map[model.WorldBloomKey]model.PlayerState) error {
	if len(records) == 0 {
		return nil
	}
	return engine.Transaction(ctx, func(tx *gorm.DB) error {
		timestampMap := make(map[int64]bool)
		userMap := make(map[string]struct {
			Name           string
			CheerfulTeamID *int
		})
		for _, record := range records {
			timestampMap[record.Timestamp] = true
			if _, exists := userMap[record.UserID]; !exists {
				userMap[record.UserID] = struct {
					Name           string
					CheerfulTeamID *int
				}{
					Name:           record.Name,
					CheerfulTeamID: record.CheerfulTeamID,
				}
			}
		}
		timeIDTable := GetTimeIDTableModel(server, eventID)
		timeIDLookup, err := batchGetOrCreateTimeIDs(tx, timeIDTable, timestampMap, 0)
		if err != nil {
			return err
		}
		usersTable := GetEventUsersTableModel(server, eventID)
		userIDKeyLookup, err := batchGetOrCreateUserIDKeys(tx, usersTable, userMap)
		if err != nil {
			return err
		}

		// Filter records: only keep those with changed (score, rank)
		var changedRecords []*model.PlayerWorldBloomRankingRecordSchema
		for _, record := range records {
			userIDKey := userIDKeyLookup[record.UserID]
			compositeKey := model.WorldBloomKey{UserIDKey: userIDKey, CharacterID: record.CharacterID}
			last, exists := prevState[compositeKey]
			if !exists || last.Score != record.Score || last.Rank != record.Rank {
				changedRecords = append(changedRecords, record)
				prevState[compositeKey] = model.PlayerState{Score: record.Score, Rank: record.Rank}
			}
		}
		
		// If no changes, skip WorldBloomTable write
		if len(changedRecords) == 0 {
			return nil
		}
		
		wlTable := GetWorldBloomTableModel(server, eventID)
		wlRecords := make([]*WorldBloomTable, 0, len(changedRecords))
		for _, record := range changedRecords {
			wlRecords = append(wlRecords, &WorldBloomTable{
				TimeID:      timeIDLookup[record.Timestamp],
				UserIDKey:   userIDKeyLookup[record.UserID],
				CharacterID: record.CharacterID,
				Score:       record.Score,
				Rank:        record.Rank,
			})
		}
		return tx.Table(wlTable.TableName()).Create(wlRecords).Error
	})
}
