package gorm

import (
	"context"
	"errors"
	"fmt"

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
	err := engine.WithContext(ctx).
		Table(eventTable.TableName()+" AS e").
		Select("t.timestamp, u.user_id, e.score, e.rank").
		Joins("INNER JOIN "+timeIDTable.TableName()+" AS t ON e.time_id = t.time_id").
		Joins("INNER JOIN "+usersTable.TableName()+" AS u ON e.user_id_key = u.user_id_key").
		Where("u.user_id = ?", userID).
		Order("t.timestamp DESC").
		Limit(1).
		First(&result).Error
	if err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch latest ranking: %w", err)
	}
	return &result, nil
}

func FetchAllRankings(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, userID string) ([]*model.RecordedRankingSchema, error) {
	eventTable := GetEventTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var results []*model.RecordedRankingSchema
	err := engine.WithContext(ctx).
		Table(eventTable.TableName()+" AS e").
		Select("t.timestamp, u.user_id, e.score, e.rank").
		Joins("INNER JOIN "+timeIDTable.TableName()+" AS t ON e.time_id = t.time_id").
		Joins("INNER JOIN "+usersTable.TableName()+" AS u ON e.user_id_key = u.user_id_key").
		Where("u.user_id = ?", userID).
		Order("t.timestamp ASC").
		Find(&results).Error
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
	err := engine.WithContext(ctx).
		Table(wlTable.TableName()+" AS w").
		Select("t.timestamp, u.user_id, w.score, w.rank, w.character_id").
		Joins("INNER JOIN "+timeIDTable.TableName()+" AS t ON w.time_id = t.time_id").
		Joins("INNER JOIN "+usersTable.TableName()+" AS u ON w.user_id_key = u.user_id_key").
		Where("u.user_id = ? AND w.character_id = ?", userID, characterID).
		Order("t.timestamp DESC").
		Limit(1).
		First(&result).Error
	if err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch latest world bloom ranking: %w", err)
	}
	return &result, nil
}

func FetchAllWorldBloomRankings(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, userID string, characterID int) ([]*model.RecordedWorldBloomRankingSchema, error) {
	wlTable := GetWorldBloomTableModel(server, eventID)
	timeIDTable := GetTimeIDTableModel(server, eventID)
	usersTable := GetEventUsersTableModel(server, eventID)
	var results []*model.RecordedWorldBloomRankingSchema
	err := engine.WithContext(ctx).
		Table(wlTable.TableName()+" AS w").
		Select("t.timestamp, u.user_id, w.score, w.rank, w.character_id").
		Joins("INNER JOIN "+timeIDTable.TableName()+" AS t ON w.time_id = t.time_id").
		Joins("INNER JOIN "+usersTable.TableName()+" AS u ON w.user_id_key = u.user_id_key").
		Where("u.user_id = ? AND w.character_id = ?", userID, characterID).
		Order("t.timestamp ASC").
		Find(&results).Error
	if err != nil {
		return nil, fmt.Errorf("failed to fetch all world bloom rankings: %w", err)
	}
	return results, nil
}

func GetOrCreateTimeID(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, timestamp int64) (int, error) {
	timeIDTable := GetTimeIDTableModel(server, eventID)
	var result TimeIDTable
	err := engine.WithContext(ctx).
		Table(timeIDTable.TableName()).
		Where("timestamp = ?", timestamp).
		First(&result).Error
	if err == nil {
		return result.TimeID, nil
	}
	if !errors.Is(err, gorm.ErrRecordNotFound) {
		return 0, fmt.Errorf("failed to query time_id: %w", err)
	}
	newRecord := &TimeIDTable{Timestamp: timestamp}
	err = engine.WithContext(ctx).
		Table(timeIDTable.TableName()).
		Create(newRecord).Error
	if err != nil {
		return 0, fmt.Errorf("failed to create time_id: %w", err)
	}
	return newRecord.TimeID, nil
}

func GetOrCreateUserIDKey(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, userID string, name string, cheerfulTeamID *int) (int, error) {
	usersTable := GetEventUsersTableModel(server, eventID)
	var result EventUsersTable
	err := engine.WithContext(ctx).
		Table(usersTable.TableName()).
		Where("user_id = ?", userID).
		First(&result).Error
	if err == nil {
		if result.Name != name || (cheerfulTeamID != nil && (result.CheerfulTeamID == nil || *result.CheerfulTeamID != *cheerfulTeamID)) {
			result.Name = name
			result.CheerfulTeamID = cheerfulTeamID
			err = engine.WithContext(ctx).
				Table(usersTable.TableName()).
				Save(&result).Error
			if err != nil {
				return 0, fmt.Errorf("failed to update user: %w", err)
			}
		}
		return result.UserIDKey, nil
	}
	if !errors.Is(err, gorm.ErrRecordNotFound) {
		return 0, fmt.Errorf("failed to query user_id_key: %w", err)
	}
	newRecord := &EventUsersTable{
		UserID:         userID,
		Name:           name,
		CheerfulTeamID: cheerfulTeamID,
	}
	err = engine.WithContext(ctx).
		Table(usersTable.TableName()).
		Create(newRecord).Error
	if err != nil {
		return 0, fmt.Errorf("failed to create user: %w", err)
	}
	return newRecord.UserIDKey, nil
}

func InsertEventRanking(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, timestamp int64, userID string, name string, score int, rank int, cheerfulTeamID *int) error {
	timeID, err := GetOrCreateTimeID(ctx, engine, server, eventID, timestamp)
	if err != nil {
		return err
	}
	userIDKey, err := GetOrCreateUserIDKey(ctx, engine, server, eventID, userID, name, cheerfulTeamID)
	if err != nil {
		return err
	}
	eventTable := GetEventTableModel(server, eventID)
	record := &EventTable{
		TimeID:    timeID,
		UserIDKey: userIDKey,
		Score:     score,
		Rank:      rank,
	}
	err = engine.WithContext(ctx).
		Table(eventTable.TableName()).
		Create(record).Error
	if err != nil {
		return fmt.Errorf("failed to insert event ranking: %w", err)
	}
	return nil
}

func InsertWorldBloomRanking(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, timestamp int64, userID string, name string, characterID int, score int, rank int, cheerfulTeamID *int) error {
	timeID, err := GetOrCreateTimeID(ctx, engine, server, eventID, timestamp)
	if err != nil {
		return err
	}
	userIDKey, err := GetOrCreateUserIDKey(ctx, engine, server, eventID, userID, name, cheerfulTeamID)
	if err != nil {
		return err
	}
	wlTable := GetWorldBloomTableModel(server, eventID)
	record := &WorldBloomTable{
		TimeID:      timeID,
		UserIDKey:   userIDKey,
		CharacterID: characterID,
		Score:       score,
		Rank:        rank,
	}
	err = engine.WithContext(ctx).
		Table(wlTable.TableName()).
		Create(record).Error
	if err != nil {
		return fmt.Errorf("failed to insert world bloom ranking: %w", err)
	}
	return nil
}

func batchGetOrCreateTimeIDs(tx *gorm.DB, timeIDTable *DynamicTimeIDTable, timestamps map[int64]bool) (map[int64]int, error) {
	timeIDLookup := make(map[int64]int)
	for timestamp := range timestamps {
		var result TimeIDTable
		err := tx.Table(timeIDTable.TableName()).
			Where("timestamp = ?", timestamp).
			First(&result).Error
		if errors.Is(err, gorm.ErrRecordNotFound) {
			newRecord := &TimeIDTable{Timestamp: timestamp}
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

func BatchInsertEventRankings(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, records []*model.PlayerEventRankingRecordSchema) error {
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
		timeIDLookup, err := batchGetOrCreateTimeIDs(tx, timeIDTable, timestampMap)
		if err != nil {
			return err
		}
		usersTable := GetEventUsersTableModel(server, eventID)
		userIDKeyLookup, err := batchGetOrCreateUserIDKeys(tx, usersTable, userMap)
		if err != nil {
			return err
		}
		eventTable := GetEventTableModel(server, eventID)
		eventRecords := make([]*EventTable, 0, len(records))
		for _, record := range records {
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

func BatchInsertWorldBloomRankings(ctx context.Context, engine *DatabaseEngine, server model.SekaiServerRegion, eventID int, records []*model.PlayerWorldBloomRankingRecordSchema) error {
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
		timeIDLookup, err := batchGetOrCreateTimeIDs(tx, timeIDTable, timestampMap)
		if err != nil {
			return err
		}
		usersTable := GetEventUsersTableModel(server, eventID)
		userIDKeyLookup, err := batchGetOrCreateUserIDKeys(tx, usersTable, userMap)
		if err != nil {
			return err
		}
		wlTable := GetWorldBloomTableModel(server, eventID)
		wlRecords := make([]*WorldBloomTable, 0, len(records))
		for _, record := range records {
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
