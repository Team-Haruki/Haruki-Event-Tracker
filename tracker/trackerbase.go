package tracker

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"

	"haruki-tracker/utils/gorm"
	"haruki-tracker/utils/logger"
	"haruki-tracker/utils/model"

	"github.com/bytedance/sonic"
	"github.com/redis/go-redis/v9"
)

type HandledRankingData struct {
	RecordTime         int64
	Rankings           []model.PlayerRankingSchema
	WorldBloomRankings []model.PlayerRankingSchema
	CharacterID        *int
}

type EventTrackerBase struct {
	server                   model.SekaiServerRegion
	eventID                  int
	eventType                model.SekaiEventType
	isEventEnded             bool
	worldBloomStatuses       map[int]model.WorldBloomChapterStatus
	isWorldBloomChapterEnded map[int]bool
	dbEngine                 *gorm.DatabaseEngine
	redisClient              *redis.Client
	apiClient                *HarukiSekaiAPIClient
	lastUpdateTime           string
	logger                   *logger.Logger
}

func NewEventTrackerBase(
	server model.SekaiServerRegion,
	eventID int,
	eventType model.SekaiEventType,
	isEventEnded bool,
	engine *gorm.DatabaseEngine,
	redisClient *redis.Client,
	apiClient *HarukiSekaiAPIClient,
	worldBloomStatuses map[int]model.WorldBloomChapterStatus,
) *EventTrackerBase {
	tracker := &EventTrackerBase{
		server:             server,
		eventID:            eventID,
		eventType:          eventType,
		isEventEnded:       isEventEnded,
		worldBloomStatuses: worldBloomStatuses,
		dbEngine:           engine,
		redisClient:        redisClient,
		apiClient:          apiClient,
		logger:             logger.NewLogger(fmt.Sprintf("HarukiEventTrackerBase%s-%d", strings.ToUpper(string(server)), eventID), "INFO", nil),
	}
	if eventType == model.SekaiEventTypeWorldBloom && worldBloomStatuses != nil && len(worldBloomStatuses) > 0 {
		tracker.isWorldBloomChapterEnded = make(map[int]bool)
		for characterID := range worldBloomStatuses {
			tracker.isWorldBloomChapterEnded[characterID] = false
		}
	}
	return tracker
}

// worldBloomStatusesEqual compares two WorldBloomChapterStatus maps for equality
func worldBloomStatusesEqual(a, b map[int]model.WorldBloomChapterStatus) bool {
	if len(a) != len(b) {
		return false
	}
	for k, v := range a {
		if bv, ok := b[k]; !ok || v != bv {
			return false
		}
	}
	return true
}

func (t *EventTrackerBase) Init(ctx context.Context) error {
	t.logger.Infof("Initializing %s %d event tracker...", t.server, t.eventID)
	if err := t.dbEngine.CreateEventTables(ctx, t.server, t.eventID, t.eventType == model.SekaiEventTypeWorldBloom); err != nil {
		return fmt.Errorf("failed to create event tables: %w", err)
	}
	t.logger.Infof("Initialized %s %d event tracker.", t.server, t.eventID)
	return nil
}

func (t *EventTrackerBase) detectCache(ctx context.Context, key string, newData []model.PlayerRankingSchema) (bool, error) {
	newDataJSON, err := sonic.Marshal(newData)
	if err != nil {
		return false, fmt.Errorf("failed to marshal new data: %w", err)
	}
	cachedDataJSON, err := t.redisClient.Get(ctx, key).Result()
	if err != nil && !errors.Is(err, redis.Nil) {
		return false, fmt.Errorf("failed to get cached data: %w", err)
	}
	if errors.Is(err, redis.Nil) || cachedDataJSON != string(newDataJSON) {
		if err := t.redisClient.Set(ctx, key, newDataJSON, 0).Err(); err != nil {
			return false, fmt.Errorf("failed to set cache: %w", err)
		}
		return false, nil
	}

	return true, nil
}

func (t *EventTrackerBase) mergeRankings(
	ctx context.Context,
	top100Rankings []model.PlayerRankingSchema,
	borderRankings []model.PlayerRankingSchema,
	cacheKey string,
) ([]model.PlayerRankingSchema, error) {
	isCached, err := t.detectCache(ctx, cacheKey, borderRankings)
	if err != nil {
		t.logger.Warnf("Failed to check cache for %s: %v, using all data", cacheKey, err)
		isCached = false
	}
	if isCached {
		return top100Rankings, nil
	}
	top100Ranks := make(map[int]bool)
	for _, r := range top100Rankings {
		if r.Rank != nil {
			top100Ranks[*r.Rank] = true
		}
	}
	result := make([]model.PlayerRankingSchema, 0, len(top100Rankings)+len(borderRankings))
	result = append(result, top100Rankings...)
	for _, r := range borderRankings {
		if r.Rank != nil && !top100Ranks[*r.Rank] {
			result = append(result, r)
		}
	}
	return result, nil
}

func (t *EventTrackerBase) handleRankingData(ctx context.Context) (*HandledRankingData, error) {
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
	if t.eventType == model.SekaiEventTypeWorldBloom {
		if top100.UserWorldBloomChapterRankings != nil && len(top100.UserWorldBloomChapterRankings) > 0 {
			for _, chapter := range top100.UserWorldBloomChapterRankings {
				if chapter.GameCharacterID == nil {
					continue
				}
				charID := *chapter.GameCharacterID
				status, exists := t.worldBloomStatuses[charID]
				if !exists {
					continue
				}
				if *chapter.IsWorldBloomChapterAggregate {
					continue
				}
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
		if border != nil && border.UserWorldBloomChapterRankingBorders != nil && len(border.UserWorldBloomChapterRankingBorders) > 0 && characterID != nil {
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
	mainCacheKey := fmt.Sprintf("%s-event-%d-main-border", t.server, t.eventID)
	rankings, err := t.mergeRankings(ctx, mainTop100Rankings, mainBorderRankings, mainCacheKey)
	if err != nil {
		return nil, fmt.Errorf("failed to merge main rankings: %w", err)
	}
	var wlRankings []model.PlayerRankingSchema
	if len(wlTop100Rankings) > 0 && characterID != nil {
		wlCacheKey := fmt.Sprintf("%s-event-%d-world-bloom-%d-border", t.server, t.eventID, *characterID)
		wlRankings, err = t.mergeRankings(ctx, wlTop100Rankings, wlBorderRankings, wlCacheKey)
		if err != nil {
			return nil, fmt.Errorf("failed to merge world bloom rankings: %w", err)
		}
	}
	return &HandledRankingData{
		RecordTime:         currentTime,
		Rankings:           rankings,
		WorldBloomRankings: wlRankings,
		CharacterID:        characterID,
	}, nil
}

func (t *EventTrackerBase) RecordRankingData(ctx context.Context, isOnlyRecordWorldBloom bool) error {
	if t.IsEventEnded() {
		t.logger.Infof("Detected event ended, skipping ranking data recording.")
		return nil
	}
	data, err := t.handleRankingData(ctx)
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
			return r.Rank != nil && *r.Rank <= 10
		}
	}
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
	t.logger.Infof("%s server tracker started inserting ranking data...", t.server)
	if !isOnlyRecordWorldBloom && len(eventRows) > 0 {
		if err := gorm.BatchInsertEventRankings(ctx, t.dbEngine, t.server, t.eventID, eventRows); err != nil {
			return fmt.Errorf("failed to insert event rankings: %w", err)
		}
	}
	if len(wlRows) > 0 {
		if err := gorm.BatchInsertWorldBloomRankings(ctx, t.dbEngine, t.server, t.eventID, wlRows); err != nil {
			return fmt.Errorf("failed to insert world bloom rankings: %w", err)
		}
	}
	t.logger.Infof("%s server tracker finished inserting ranking data.", t.server)
	return nil
}

func (t *EventTrackerBase) SetEventEnded(ended bool) {
	t.isEventEnded = ended
}

func (t *EventTrackerBase) IsEventEnded() bool {
	return t.isEventEnded
}

func (t *EventTrackerBase) GetEventID() int {
	return t.eventID
}

func (t *EventTrackerBase) SetWorldBloomChapterEnded(characterID int, ended bool) {
	t.isWorldBloomChapterEnded[characterID] = ended
}

func (t *EventTrackerBase) IsWorldBloomChapterEnded(characterID int) bool {
	return t.isWorldBloomChapterEnded[characterID]
}

func (t *EventTrackerBase) GetWorldBloomChapterStatus() map[int]model.WorldBloomChapterStatus {
	return t.worldBloomStatuses
}

func (t *EventTrackerBase) SetWorldBloomChapterStatus(statuses map[int]model.WorldBloomChapterStatus) {
	t.worldBloomStatuses = statuses
}
