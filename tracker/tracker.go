package tracker

import (
	"context"
	"fmt"
	"strings"
	"time"

	"haruki-tracker/utils/gorm"
	"haruki-tracker/utils/logger"
	"haruki-tracker/utils/model"
)

// EventTracker tracks event rankings and stores them in the database
type EventTracker struct {
	server                   model.SekaiServerRegion
	eventID                  int
	eventType                model.SekaiEventType
	isEventEnded             bool
	worldBloomStatuses       map[int]model.WorldBloomChapterStatus
	isWorldBloomChapterEnded map[int]bool
	engine                   *gorm.DatabaseEngine
	apiClient                *HarukiSekaiAPIClient
	lastUpdateTime           string
	logger                   *logger.Logger
}

// NewEventTracker creates a new EventTracker instance
func NewEventTracker(
	server model.SekaiServerRegion,
	eventID int,
	eventType model.SekaiEventType,
	engine *gorm.DatabaseEngine,
	apiClient *HarukiSekaiAPIClient,
	worldBloomStatuses map[int]model.WorldBloomChapterStatus,
) *EventTracker {
	tracker := &EventTracker{
		server:             server,
		eventID:            eventID,
		eventType:          eventType,
		isEventEnded:       false,
		worldBloomStatuses: worldBloomStatuses,
		engine:             engine,
		apiClient:          apiClient,
		logger:             logger.NewLogger(fmt.Sprintf("HarukiEventTracker%s-%d", strings.ToUpper(string(server)), eventID), "INFO", nil),
	}

	if eventType == model.SekaiEventTypeWorldBloom && worldBloomStatuses != nil {
		tracker.isWorldBloomChapterEnded = make(map[int]bool)
		for characterID := range worldBloomStatuses {
			tracker.isWorldBloomChapterEnded[characterID] = false
		}
	}

	return tracker
}

// Init initializes the event tracker and creates necessary database tables
func (t *EventTracker) Init(ctx context.Context) error {
	t.logger.Infof("Initializing %s %d event tracker...", t.server, t.eventID)
	if err := t.engine.CreateEventTables(ctx, t.server, t.eventID); err != nil {
		return fmt.Errorf("failed to create event tables: %w", err)
	}
	t.logger.Infof("Initialized %s %d event tracker.", t.server, t.eventID)
	return nil
}

// HandledRankingData represents processed ranking data
type HandledRankingData struct {
	RecordTime         int64
	Rankings           []model.PlayerRankingSchema
	WorldBloomRankings []model.PlayerRankingSchema
	CharacterID        *int
}

// MergeRankings merges top100 and border rankings, removing duplicates
func (t *EventTracker) MergeRankings(
	top100Rankings []model.PlayerRankingSchema,
	borderRankings []model.PlayerRankingSchema,
) []model.PlayerRankingSchema {
	// Create a set of ranks from top100
	top100Ranks := make(map[int]bool)
	for _, r := range top100Rankings {
		if r.Rank != nil {
			top100Ranks[*r.Rank] = true
		}
	}

	// Merge rankings, avoiding duplicates
	result := make([]model.PlayerRankingSchema, 0, len(top100Rankings)+len(borderRankings))
	result = append(result, top100Rankings...)

	for _, r := range borderRankings {
		if r.Rank != nil && !top100Ranks[*r.Rank] {
			result = append(result, r)
		}
	}

	return result
}

// HandleRankingData fetches and processes ranking data from the API
func (t *EventTracker) HandleRankingData(ctx context.Context) (*HandledRankingData, error) {
	top100, err := t.apiClient.GetTop100(ctx, t.eventID, t.server)
	if err != nil {
		t.logger.Errorf("Warning: Failed to get top100 rankings: %v", err)
		return nil, err
	}

	border, err := t.apiClient.GetBorder(ctx, t.eventID, t.server)
	if err != nil {
		t.logger.Errorf("Warning: Failed to get border rankings: %v", err)
		return nil, err
	}

	if top100 == nil {
		t.logger.Errorf("Warning: Haruki Sekai API error, skipping tracking...")
		return nil, fmt.Errorf("top100 response is nil")
	}

	currentTime := time.Now().Unix()
	var mainTop100Rankings, mainBorderRankings []model.PlayerRankingSchema
	var characterID *int
	var wlTop100Rankings, wlBorderRankings []model.PlayerRankingSchema

	if top100.Rankings != nil {
		mainTop100Rankings = top100.Rankings
	}
	if border != nil && border.BorderRankings != nil {
		mainBorderRankings = border.BorderRankings
	}

	// Handle World Bloom rankings
	if t.eventType == model.SekaiEventTypeWorldBloom {
		if top100.UserWorldBloomChapterRankings != nil {
			for _, chapter := range top100.UserWorldBloomChapterRankings {
				if chapter.GameCharacterID == nil {
					continue
				}

				charID := *chapter.GameCharacterID
				status, exists := t.worldBloomStatuses[charID]
				if !exists {
					continue
				}

				// Check if we should track this chapter
				chapterEnded := t.isWorldBloomChapterEnded[charID]
				if (status.ChapterStatus == model.SekaiEventStatusEnded && !chapterEnded) ||
					status.ChapterStatus == model.SekaiEventStatusOngoing {
					characterID = &charID
					if chapter.Rankings != nil {
						wlTop100Rankings = chapter.Rankings
					}
					break
				}
			}
		}

		// Find matching border rankings
		if border != nil && border.UserWorldBloomChapterRankingBorders != nil && characterID != nil {
			for _, chapter := range border.UserWorldBloomChapterRankingBorders {
				if chapter.GameCharacterID != nil && *chapter.GameCharacterID == *characterID {
					if chapter.BorderRankings != nil {
						wlBorderRankings = chapter.BorderRankings
					}
					break
				}
			}
		}
	}

	// Merge rankings
	rankings := t.MergeRankings(mainTop100Rankings, mainBorderRankings)
	var wlRankings []model.PlayerRankingSchema
	if len(wlTop100Rankings) > 0 {
		wlRankings = t.MergeRankings(wlTop100Rankings, wlBorderRankings)
	}

	return &HandledRankingData{
		RecordTime:         currentTime,
		Rankings:           rankings,
		WorldBloomRankings: wlRankings,
		CharacterID:        characterID,
	}, nil
}

// RecordRankingData records ranking data to the database
func (t *EventTracker) RecordRankingData(ctx context.Context, isOnlyRecordWorldBloom bool) error {
	data, err := t.HandleRankingData(ctx)
	if err != nil {
		return err
	}

	if data == nil {
		return nil
	}

	currentTimeMinute := time.Now().Format("01/02 15:04")
	var filterFunc func(*model.PlayerRankingSchema) bool

	if currentTimeMinute != t.lastUpdateTime {
		filterFunc = func(r *model.PlayerRankingSchema) bool { return true }
		t.lastUpdateTime = currentTimeMinute
	} else {
		filterFunc = func(r *model.PlayerRankingSchema) bool {
			return r.Rank != nil && *r.Rank == 1
		}
	}

	// Prepare event ranking records
	eventRows := make([]*model.PlayerEventRankingRecordSchema, 0)
	for i := range data.Rankings {
		r := &data.Rankings[i]
		if !filterFunc(r) {
			continue
		}

		if r.UserID == nil || r.Score == nil || r.Rank == nil || r.Name == nil {
			continue
		}

		var cheerfulTeamID *int
		if r.UserCheerfulCarnival != nil && r.UserCheerfulCarnival.CheerfulCarnivalTeamID != nil {
			cheerfulTeamID = r.UserCheerfulCarnival.CheerfulCarnivalTeamID
		}

		eventRows = append(eventRows, &model.PlayerEventRankingRecordSchema{
			Timestamp:      data.RecordTime,
			UserID:         fmt.Sprintf("%d", *r.UserID),
			Name:           *r.Name,
			Score:          *r.Score,
			Rank:           *r.Rank,
			CheerfulTeamID: cheerfulTeamID,
		})
	}

	// Prepare World Bloom records
	var wlRows []*model.PlayerWorldBloomRankingRecordSchema
	if len(data.WorldBloomRankings) > 0 && data.CharacterID != nil {
		wlRows = make([]*model.PlayerWorldBloomRankingRecordSchema, 0)
		for i := range data.WorldBloomRankings {
			r := &data.WorldBloomRankings[i]
			if r.UserID == nil || r.Score == nil || r.Rank == nil || r.Name == nil {
				continue
			}

			var cheerfulTeamID *int
			if r.UserCheerfulCarnival != nil && r.UserCheerfulCarnival.CheerfulCarnivalTeamID != nil {
				cheerfulTeamID = r.UserCheerfulCarnival.CheerfulCarnivalTeamID
			}

			wlRows = append(wlRows, &model.PlayerWorldBloomRankingRecordSchema{
				PlayerEventRankingRecordSchema: model.PlayerEventRankingRecordSchema{
					Timestamp:      data.RecordTime,
					UserID:         fmt.Sprintf("%d", *r.UserID),
					Name:           *r.Name,
					Score:          *r.Score,
					Rank:           *r.Rank,
					CheerfulTeamID: cheerfulTeamID,
				},
				CharacterID: *data.CharacterID,
			})
		}
	}

	// Insert data into database
	t.logger.Infof("%s server tracker started inserting ranking data...", t.server)

	// Insert event rankings
	if !isOnlyRecordWorldBloom && len(eventRows) > 0 {
		if err := gorm.BatchInsertEventRankings(ctx, t.engine, t.server, t.eventID, eventRows); err != nil {
			return fmt.Errorf("failed to insert event rankings: %w", err)
		}
	}

	// Insert World Bloom rankings
	if len(wlRows) > 0 {
		if err := gorm.BatchInsertWorldBloomRankings(ctx, t.engine, t.server, t.eventID, wlRows); err != nil {
			return fmt.Errorf("failed to insert world bloom rankings: %w", err)
		}
	}

	t.logger.Infof("%s server tracker finished inserting ranking data.", t.server)

	return nil
}

// SetEventEnded marks the event as ended
func (t *EventTracker) SetEventEnded(ended bool) {
	t.isEventEnded = ended
}

// IsEventEnded returns whether the event has ended
func (t *EventTracker) IsEventEnded() bool {
	return t.isEventEnded
}

// SetWorldBloomChapterEnded marks a World Bloom chapter as ended
func (t *EventTracker) SetWorldBloomChapterEnded(characterID int, ended bool) {
	if t.isWorldBloomChapterEnded != nil {
		t.isWorldBloomChapterEnded[characterID] = ended
	}
}
