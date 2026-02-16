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

	"github.com/redis/go-redis/v9"
)

type HandledRankingData struct {
	RecordTime         int64
	Rankings           []model.PlayerRankingSchema
	WorldBloomRankings []model.PlayerRankingSchema
	CharacterID        *int
}

type EventTrackerBase struct {
	server                     model.SekaiServerRegion
	eventID                    int
	eventType                  model.SekaiEventType
	isEventEnded               bool
	worldBloomStatuses         map[int]model.WorldBloomChapterStatus
	isWorldBloomChapterEnded   map[int]bool
	secondLevelTrackType       model.SecondLevelEventTrackType
	rangeTrackLowerRank        *int
	rangeTrackUpperRank        *int
	SpecificTrackRanks         *[]int
	trackSpecificPlayer        *bool
	trackSpecificPlayerUserIDs *[]string
	dbEngine                   *gorm.DatabaseEngine
	redisClient                *redis.Client
	apiClient                  *HarukiSekaiAPIClient
	lastUpdateTime             string
	logger                     *logger.Logger
	prevEventState             map[int]model.PlayerState
	prevWorldBloomState        map[model.WorldBloomKey]model.PlayerState
}

func NewEventTrackerBase(
	server model.SekaiServerRegion,
	eventID int,
	eventType model.SekaiEventType,
	isEventEnded bool,
	secondLevelTrackType model.SecondLevelEventTrackType,
	rangeTrackLowerRank *int,
	rangeTrackUpperRank *int,
	specificTrackRanks *[]int,
	trackSpecificPlayer *bool,
	trackSpecificPlayerUserIDs *[]string,
	engine *gorm.DatabaseEngine,
	redisClient *redis.Client,
	apiClient *HarukiSekaiAPIClient,
	worldBloomStatuses map[int]model.WorldBloomChapterStatus,
) *EventTrackerBase {
	tracker := &EventTrackerBase{
		server:                     server,
		eventID:                    eventID,
		eventType:                  eventType,
		isEventEnded:               isEventEnded,
		worldBloomStatuses:         worldBloomStatuses,
		secondLevelTrackType:       secondLevelTrackType,
		rangeTrackLowerRank:        rangeTrackLowerRank,
		rangeTrackUpperRank:        rangeTrackUpperRank,
		SpecificTrackRanks:         specificTrackRanks,
		trackSpecificPlayer:        trackSpecificPlayer,
		trackSpecificPlayerUserIDs: trackSpecificPlayerUserIDs,
		dbEngine:                   engine,
		redisClient:                redisClient,
		apiClient:                  apiClient,
		logger:                     logger.NewLogger(fmt.Sprintf("HarukiEventTrackerBase%s-Event%d", strings.ToUpper(string(server)), eventID), "INFO", nil),
		prevEventState:             make(map[int]model.PlayerState),
		prevWorldBloomState:        make(map[model.WorldBloomKey]model.PlayerState),
	}
	if eventType == model.SekaiEventTypeWorldBloom && worldBloomStatuses != nil && len(worldBloomStatuses) > 0 {
		tracker.isWorldBloomChapterEnded = make(map[int]bool)
		for characterID := range worldBloomStatuses {
			tracker.isWorldBloomChapterEnded[characterID] = false
		}
	}
	return tracker
}

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

func (t *EventTrackerBase) detectCache(ctx context.Context, key string, newHash [32]byte) (bool, error) {
	cachedHashStr, err := t.redisClient.Get(ctx, key).Result()
	if err != nil && !errors.Is(err, redis.Nil) {
		return false, fmt.Errorf("failed to get cached data: %w", err)
	}
	if errors.Is(err, redis.Nil) {
		t.logger.Debugf("Cache miss: key %s not found, setting cache for %s %d event tracker", key, t.server, t.eventID)
		if err := t.redisClient.Set(ctx, key, fmt.Sprintf("%x", newHash), 0).Err(); err != nil {
			return false, fmt.Errorf("failed to set cache: %w", err)
		}
		return false, nil
	}
	if cachedHashStr != fmt.Sprintf("%x", newHash) {
		t.logger.Debugf("Cache miss: data changed for key %s, setting cache for %s %d event tracker", key, t.server, t.eventID)
		if err := t.redisClient.Set(ctx, key, fmt.Sprintf("%x", newHash), 0).Err(); err != nil {
			return false, fmt.Errorf("failed to set cache: %w", err)
		}
		return false, nil
	}

	t.logger.Debugf("Cache hit: key %s found for %s %d event tracker", key, t.server, t.eventID)
	return true, nil
}

func (t *EventTrackerBase) mergeRankings(
	ctx context.Context,
	top100Rankings []model.PlayerRankingSchema,
	borderRankings []model.PlayerRankingSchema,
	borderHash [32]byte,
	cacheKey string,
) ([]model.PlayerRankingSchema, error) {
	isCached, err := t.detectCache(ctx, cacheKey, borderHash)
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

func (t *EventTrackerBase) extractMainRankings(top100 *model.Top100RankingResponse, border *model.BorderRankingResponse) ([]model.PlayerRankingSchema, []model.PlayerRankingSchema) {
	var mainTop100Rankings, mainBorderRankings []model.PlayerRankingSchema
	if top100.Rankings != nil {
		mainTop100Rankings = top100.Rankings
	}
	if border != nil && border.BorderRankings != nil {
		mainBorderRankings = border.BorderRankings
	}
	return mainTop100Rankings, mainBorderRankings
}

func (t *EventTrackerBase) extractWorldBloomRankings(top100 *model.Top100RankingResponse, border *model.BorderRankingResponse) ([]model.PlayerRankingSchema, []model.PlayerRankingSchema, *int) {
	var wlTop100Rankings, wlBorderRankings []model.PlayerRankingSchema
	var characterID *int

	if top100.UserWorldBloomChapterRankings == nil || len(top100.UserWorldBloomChapterRankings) == 0 {
		return wlTop100Rankings, wlBorderRankings, characterID
	}

	for _, chapter := range top100.UserWorldBloomChapterRankings {
		charID, rankings := t.processWorldBloomChapter(chapter)
		if charID != nil {
			characterID = charID
			wlTop100Rankings = rankings
			break
		}
	}

	if border != nil && border.UserWorldBloomChapterRankingBorders != nil && characterID != nil {
		wlBorderRankings = t.extractWorldBloomBorderRankings(border.UserWorldBloomChapterRankingBorders, *characterID)
	}

	return wlTop100Rankings, wlBorderRankings, characterID
}

func (t *EventTrackerBase) processWorldBloomChapter(chapter model.UserWorldBloomChapterRanking) (*int, []model.PlayerRankingSchema) {
	if chapter.GameCharacterID == nil {
		return nil, nil
	}

	charID := *chapter.GameCharacterID
	status, exists := t.worldBloomStatuses[charID]
	if !exists {
		return nil, nil
	}

	if *chapter.IsWorldBloomChapterAggregate {
		return nil, nil
	}

	chapterEnded := t.isWorldBloomChapterEnded[charID]
	if (status.ChapterStatus == model.SekaiEventStatusEnded && !chapterEnded) ||
		status.ChapterStatus == model.SekaiEventStatusOngoing {
		if chapter.Rankings != nil {
			return &charID, chapter.Rankings
		}
	}

	return nil, nil
}

func (t *EventTrackerBase) extractWorldBloomBorderRankings(borders []model.UserWorldBloomChapterRankingBorder, characterID int) []model.PlayerRankingSchema {
	for _, chapter := range borders {
		if chapter.GameCharacterID != nil && *chapter.GameCharacterID == characterID {
			if chapter.BorderRankings != nil {
				return chapter.BorderRankings
			}
			break
		}
	}
	return nil
}

func (t *EventTrackerBase) handleRankingData(ctx context.Context) (*HandledRankingData, error) {
	top100, err := t.apiClient.GetTop100(ctx, t.eventID, t.server)
	if err != nil {
		t.logger.Errorf("Warning: Failed to get top100 rankings: %v", err)
		return nil, err
	}
	borderHash, border, err := t.apiClient.GetBorder(ctx, t.eventID, t.server)
	if err != nil {
		t.logger.Errorf("Warning: Failed to get border rankings: %v", err)
		return nil, err
	}
	t.logger.Debugf("Border response hash: %x", borderHash)
	if top100 == nil {
		t.logger.Errorf("Warning: Haruki Sekai API error, skipping tracking...")
		return nil, fmt.Errorf("top100 response is nil")
	}

	currentTime := time.Now().Unix()
	mainTop100Rankings, mainBorderRankings := t.extractMainRankings(top100, border)

	var wlTop100Rankings, wlBorderRankings []model.PlayerRankingSchema
	var characterID *int
	if t.eventType == model.SekaiEventTypeWorldBloom {
		wlTop100Rankings, wlBorderRankings, characterID = t.extractWorldBloomRankings(top100, border)
	}

	mainCacheKey := fmt.Sprintf("%s-event-%d-main-border", t.server, t.eventID)
	rankings, err := t.mergeRankings(ctx, mainTop100Rankings, mainBorderRankings, borderHash, mainCacheKey)
	if err != nil {
		return nil, fmt.Errorf("failed to merge main rankings: %w", err)
	}

	var wlRankings []model.PlayerRankingSchema
	if len(wlTop100Rankings) > 0 && characterID != nil {
		wlCacheKey := fmt.Sprintf("%s-event-%d-world-bloom-%d-border", t.server, t.eventID, *characterID)
		wlRankings, err = t.mergeRankings(ctx, wlTop100Rankings, wlBorderRankings, borderHash, wlCacheKey)
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

func (t *EventTrackerBase) getFilterFunc(currentTimeMinute string) func(*model.PlayerRankingSchema) bool {
	if currentTimeMinute != t.lastUpdateTime {
		t.lastUpdateTime = currentTimeMinute
		return func(r *model.PlayerRankingSchema) bool { return true }
	}
	return func(r *model.PlayerRankingSchema) bool {
		if *r.Rank > 100 {
			return true
		}
		if t.checkRange(r) || t.checkSpecificRanks(r) || t.checkSpecificPlayers(r) {
			return true
		}
		return false
	}
}

func (t *EventTrackerBase) checkRange(r *model.PlayerRankingSchema) bool {
	if t.secondLevelTrackType != model.SecondLevelEventTrackTypeRange {
		return false
	}
	if t.rangeTrackUpperRank == nil || t.rangeTrackLowerRank == nil {
		return false
	}
	lower := *t.rangeTrackLowerRank
	upper := *t.rangeTrackUpperRank
	if lower > upper {
		lower, upper = upper, lower
	}
	if r.Rank == nil {
		return false
	}
	return *r.Rank >= lower && *r.Rank <= upper
}

func (t *EventTrackerBase) checkSpecificRanks(r *model.PlayerRankingSchema) bool {
	if t.secondLevelTrackType != model.SecondLevelEventTrackTypeSpecific {
		return false
	}
	if t.SpecificTrackRanks == nil || r.Rank == nil {
		return false
	}
	for _, rank := range *t.SpecificTrackRanks {
		if *r.Rank == rank {
			return true
		}
	}
	return false
}

func (t *EventTrackerBase) checkSpecificPlayers(r *model.PlayerRankingSchema) bool {
	if t.trackSpecificPlayer == nil || !*t.trackSpecificPlayer {
		return false
	}
	if t.trackSpecificPlayerUserIDs == nil || r.UserID == nil {
		return false
	}
	for _, userID := range *t.trackSpecificPlayerUserIDs {
		if fmt.Sprintf("%d", *r.UserID) == userID {
			return true
		}
	}
	return false
}

func (t *EventTrackerBase) extractCheerfulTeamID(r *model.PlayerRankingSchema) *int {
	if r.UserCheerfulCarnival != nil && r.UserCheerfulCarnival.CheerfulCarnivalTeamID != nil {
		return r.UserCheerfulCarnival.CheerfulCarnivalTeamID
	}
	return nil
}

func (t *EventTrackerBase) buildEventRows(data *HandledRankingData, filterFunc func(*model.PlayerRankingSchema) bool) []*model.PlayerEventRankingRecordSchema {
	eventRows := make([]*model.PlayerEventRankingRecordSchema, 0)
	for i := range data.Rankings {
		r := &data.Rankings[i]
		if !filterFunc(r) {
			continue
		}
		if r.UserID == nil || r.Score == nil || r.Rank == nil || r.Name == nil {
			continue
		}
		eventRows = append(eventRows, &model.PlayerEventRankingRecordSchema{
			Timestamp:      data.RecordTime,
			UserID:         fmt.Sprintf("%d", *r.UserID),
			Name:           *r.Name,
			Score:          *r.Score,
			Rank:           *r.Rank,
			CheerfulTeamID: t.extractCheerfulTeamID(r),
		})
	}
	return eventRows
}

func (t *EventTrackerBase) buildWorldBloomRows(data *HandledRankingData) []*model.PlayerWorldBloomRankingRecordSchema {
	if len(data.WorldBloomRankings) == 0 || data.CharacterID == nil {
		return nil
	}

	wlRows := make([]*model.PlayerWorldBloomRankingRecordSchema, 0)
	for i := range data.WorldBloomRankings {
		r := &data.WorldBloomRankings[i]
		if r.UserID == nil || r.Score == nil || r.Rank == nil || r.Name == nil {
			continue
		}
		wlRows = append(wlRows, &model.PlayerWorldBloomRankingRecordSchema{
			PlayerEventRankingRecordSchema: model.PlayerEventRankingRecordSchema{
				Timestamp:      data.RecordTime,
				UserID:         fmt.Sprintf("%d", *r.UserID),
				Name:           *r.Name,
				Score:          *r.Score,
				Rank:           *r.Rank,
				CheerfulTeamID: t.extractCheerfulTeamID(r),
			},
			CharacterID: *data.CharacterID,
		})
	}
	return wlRows
}

func (t *EventTrackerBase) RecordRankingData(ctx context.Context, isOnlyRecordWorldBloom bool) error {
	if t.IsEventEnded() {
		t.logger.Infof("Detected event ended, skipping ranking data recording.")
		return nil
	}
	
	// Always write heartbeat first with current time
	currentTime := time.Now().Unix()
	heartbeatWritten := false
	
	data, err := t.handleRankingData(ctx)
	if err != nil {
		// On API error, write heartbeat with status=1 and return
		t.logger.Warnf("API error, writing heartbeat with status=1: %v", err)
		if heartbeatErr := gorm.WriteHeartbeat(ctx, t.dbEngine, t.server, t.eventID, currentTime, 1); heartbeatErr != nil {
			t.logger.Errorf("Failed to write heartbeat on API error: %v", heartbeatErr)
			return fmt.Errorf("API error and heartbeat write failed: %w; original error: %w", heartbeatErr, err)
		}
		return err
	}
	if data == nil {
		return nil
	}

	currentTimeMinute := time.Now().Format("01/02 15:04")
	filterFunc := t.getFilterFunc(currentTimeMinute)

	eventRows := t.buildEventRows(data, filterFunc)
	wlRows := t.buildWorldBloomRows(data)

	t.logger.Infof("%s server tracker started inserting ranking data...", t.server)
	
	// Handle event rankings
	if !isOnlyRecordWorldBloom {
		if len(eventRows) > 0 {
			if err := gorm.BatchInsertEventRankings(ctx, t.dbEngine, t.server, t.eventID, eventRows, t.prevEventState); err != nil {
				return fmt.Errorf("failed to insert event rankings: %w", err)
			}
			heartbeatWritten = true // BatchInsertEventRankings writes heartbeat via batchGetOrCreateTimeIDs
		}
	}
	
	// Handle world bloom rankings
	if len(wlRows) > 0 {
		if err := gorm.BatchInsertWorldBloomRankings(ctx, t.dbEngine, t.server, t.eventID, wlRows, t.prevWorldBloomState); err != nil {
			return fmt.Errorf("failed to insert world bloom rankings: %w", err)
		}
		heartbeatWritten = true // BatchInsertWorldBloomRankings writes heartbeat via batchGetOrCreateTimeIDs
	}
	
	// If no data was written (all records filtered out by deduplication), still write heartbeat
	if !heartbeatWritten {
		if heartbeatErr := gorm.WriteHeartbeat(ctx, t.dbEngine, t.server, t.eventID, currentTime, 0); heartbeatErr != nil {
			t.logger.Errorf("Failed to write heartbeat with no data changes: %v", heartbeatErr)
			return fmt.Errorf("failed to write heartbeat: %w", heartbeatErr)
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
