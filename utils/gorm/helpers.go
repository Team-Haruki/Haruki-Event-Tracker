package gorm

import (
	"context"
	"fmt"

	"haruki-tracker/utils/model"

	"gorm.io/gorm"
)

// GetUserNameData fetches user name data from the event names table
func GetUserNameData(ctx context.Context, engine *DatabaseEngine, eventID int, userID string) (*model.RecordedUserNameSchema, error) {
	table := GetEventNamesTableModel(eventID)
	var result EventNamesTable

	err := engine.WithContext(ctx).
		Table(table.TableName()).
		Where("user_id = ?", userID).
		First(&result).Error

	if err != nil {
		if err == gorm.ErrRecordNotFound {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch user name: %w", err)
	}

	return &model.RecordedUserNameSchema{
		UserID:         result.UserID,
		Name:           result.Name,
		CheerfulTeamID: result.CheerfulTeamID,
	}, nil
}

// FetchLatestRanking fetches the latest ranking record for a user
func FetchLatestRanking(ctx context.Context, engine *DatabaseEngine, eventID int, userID string) (*model.RecordedRankingSchema, error) {
	table := GetEventTableModel(eventID)
	var result EventTable

	err := engine.WithContext(ctx).
		Table(table.TableName()).
		Where("user_id = ?", userID).
		Order("timestamp DESC").
		Limit(1).
		First(&result).Error

	if err != nil {
		if err == gorm.ErrRecordNotFound {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch latest ranking: %w", err)
	}

	return &model.RecordedRankingSchema{
		Timestamp: result.Timestamp,
		UserID:    result.UserID,
		Score:     result.Score,
		Rank:      result.Rank,
	}, nil
}

// FetchAllRankings fetches all ranking records for a user
func FetchAllRankings(ctx context.Context, engine *DatabaseEngine, eventID int, userID string) ([]*model.RecordedRankingSchema, error) {
	table := GetEventTableModel(eventID)
	var results []EventTable

	err := engine.WithContext(ctx).
		Table(table.TableName()).
		Where("user_id = ?", userID).
		Order("timestamp ASC").
		Find(&results).Error

	if err != nil {
		return nil, fmt.Errorf("failed to fetch all rankings: %w", err)
	}

	rankings := make([]*model.RecordedRankingSchema, 0, len(results))
	for _, r := range results {
		rankings = append(rankings, &model.RecordedRankingSchema{
			Timestamp: r.Timestamp,
			UserID:    r.UserID,
			Score:     r.Score,
			Rank:      r.Rank,
		})
	}

	return rankings, nil
}

// FetchLatestWorldBloomRanking fetches the latest world bloom ranking record for a user and character
func FetchLatestWorldBloomRanking(ctx context.Context, engine *DatabaseEngine, eventID int, userID string, characterID int) (*model.RecordedWorldBloomRankingSchema, error) {
	table := GetWorldBloomTableModel(eventID)
	var result WorldBloomTable

	err := engine.WithContext(ctx).
		Table(table.TableName()).
		Where("user_id = ? AND character_id = ?", userID, characterID).
		Order("timestamp DESC").
		Limit(1).
		First(&result).Error

	if err != nil {
		if err == gorm.ErrRecordNotFound {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to fetch latest world bloom ranking: %w", err)
	}

	return &model.RecordedWorldBloomRankingSchema{
		RecordedRankingSchema: model.RecordedRankingSchema{
			Timestamp: result.Timestamp,
			UserID:    result.UserID,
			Score:     result.Score,
			Rank:      result.Rank,
		},
		CharacterID: &result.CharacterID,
	}, nil
}

// FetchAllWorldBloomRankings fetches all world bloom ranking records for a user and character
func FetchAllWorldBloomRankings(ctx context.Context, engine *DatabaseEngine, eventID int, userID string, characterID int) ([]*model.RecordedWorldBloomRankingSchema, error) {
	table := GetWorldBloomTableModel(eventID)
	var results []WorldBloomTable

	err := engine.WithContext(ctx).
		Table(table.TableName()).
		Where("user_id = ? AND character_id = ?", userID, characterID).
		Order("timestamp ASC").
		Find(&results).Error

	if err != nil {
		return nil, fmt.Errorf("failed to fetch all world bloom rankings: %w", err)
	}

	rankings := make([]*model.RecordedWorldBloomRankingSchema, 0, len(results))
	for _, r := range results {
		charID := r.CharacterID
		rankings = append(rankings, &model.RecordedWorldBloomRankingSchema{
			RecordedRankingSchema: model.RecordedRankingSchema{
				Timestamp: r.Timestamp,
				UserID:    r.UserID,
				Score:     r.Score,
				Rank:      r.Rank,
			},
			CharacterID: &charID,
		})
	}

	return rankings, nil
}

// InsertEventRanking inserts or updates an event ranking record
func InsertEventRanking(ctx context.Context, engine *DatabaseEngine, eventID int, record *EventTable) error {
	table := GetEventTableModel(eventID)

	err := engine.WithContext(ctx).
		Table(table.TableName()).
		Create(record).Error

	if err != nil {
		return fmt.Errorf("failed to insert event ranking: %w", err)
	}

	return nil
}

// InsertWorldBloomRanking inserts or updates a world bloom ranking record
func InsertWorldBloomRanking(ctx context.Context, engine *DatabaseEngine, eventID int, record *WorldBloomTable) error {
	table := GetWorldBloomTableModel(eventID)

	err := engine.WithContext(ctx).
		Table(table.TableName()).
		Create(record).Error

	if err != nil {
		return fmt.Errorf("failed to insert world bloom ranking: %w", err)
	}

	return nil
}

// UpsertEventName inserts or updates an event name record
func UpsertEventName(ctx context.Context, engine *DatabaseEngine, eventID int, record *EventNamesTable) error {
	table := GetEventNamesTableModel(eventID)

	// GORM's Save will insert or update based on primary key
	err := engine.WithContext(ctx).
		Table(table.TableName()).
		Save(record).Error

	if err != nil {
		return fmt.Errorf("failed to upsert event name: %w", err)
	}

	return nil
}

// BatchInsertEventRankings inserts multiple event ranking records in a transaction
func BatchInsertEventRankings(ctx context.Context, engine *DatabaseEngine, eventID int, records []*EventTable) error {
	if len(records) == 0 {
		return nil
	}

	table := GetEventTableModel(eventID)

	return engine.Transaction(ctx, func(tx *gorm.DB) error {
		return tx.Table(table.TableName()).Create(records).Error
	})
}

// BatchInsertWorldBloomRankings inserts multiple world bloom ranking records in a transaction
func BatchInsertWorldBloomRankings(ctx context.Context, engine *DatabaseEngine, eventID int, records []*WorldBloomTable) error {
	if len(records) == 0 {
		return nil
	}

	table := GetWorldBloomTableModel(eventID)

	return engine.Transaction(ctx, func(tx *gorm.DB) error {
		return tx.Table(table.TableName()).Create(records).Error
	})
}
