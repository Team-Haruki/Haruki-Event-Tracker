package tracker

import (
	"context"
	"encoding/json"
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
	WorldBloomRankings map[int][]model.PlayerRankingSchema
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
	prevEventState           map[int]model.PlayerState
	prevWorldBloomState      map[model.WorldBloomKey]model.PlayerState
	prevRankState            map[int]model.RankState
	prevUserState            map[string]model.PlayerState
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
		server:              server,
		eventID:             eventID,
		eventType:           eventType,
		isEventEnded:        isEventEnded,
		worldBloomStatuses:  worldBloomStatuses,
		dbEngine:            engine,
		redisClient:         redisClient,
		apiClient:           apiClient,
		logger:              logger.NewLogger(fmt.Sprintf("HarukiEventTrackerBase%s-Event%d", strings.ToUpper(string(server)), eventID), "INFO", nil),
		prevEventState:      make(map[int]model.PlayerState),
		prevWorldBloomState: make(map[model.WorldBloomKey]model.PlayerState),
		prevRankState:       make(map[int]model.RankState),
		prevUserState:       make(map[string]model.PlayerState),
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

func (t *EventTrackerBase) getKey(suffix string) string {
	return fmt.Sprintf("haruki:tracker:%s:%d:%s", t.server, t.eventID, suffix)
}

func (t *EventTrackerBase) loadStateFromRedis(ctx context.Context) {
	rankStateKey := t.getKey("rank_state")
	rankStateData, err := t.redisClient.HGetAll(ctx, rankStateKey).Result()
	if err == nil {
		for k, v := range rankStateData {
			var state model.RankState
			if err := json.Unmarshal([]byte(v), &state); err == nil {
				var rank int
				fmt.Sscanf(k, "%d", &rank)
				t.prevRankState[rank] = state
			}
		}
		t.logger.Infof("Loaded %d rank states from Redis", len(t.prevRankState))
	} else {
		t.logger.Warnf("Failed to load rank state from Redis: %v", err)
	}
	userStateKey := t.getKey("user_state")
	userStateData, err := t.redisClient.HGetAll(ctx, userStateKey).Result()
	if err == nil {
		for k, v := range userStateData {
			var state model.PlayerState
			if err := json.Unmarshal([]byte(v), &state); err == nil {
				t.prevUserState[k] = state
			}
		}
		t.logger.Infof("Loaded %d user states from Redis", len(t.prevUserState))
	} else {
		t.logger.Warnf("Failed to load user state from Redis: %v", err)
	}
}

func (t *EventTrackerBase) saveStateToRedis(ctx context.Context, changedRanks map[int]model.RankState, changedUsers map[string]model.PlayerState) {
	pipe := t.redisClient.Pipeline()
	ttl := 24 * time.Hour * 14
	if len(changedRanks) > 0 {
		rankStateKey := t.getKey("rank_state")
		params := make([]interface{}, 0, len(changedRanks)*2)
		for k, v := range changedRanks {
			data, _ := json.Marshal(v)
			params = append(params, fmt.Sprintf("%d", k), string(data))
		}
		pipe.HSet(ctx, rankStateKey, params...)
		pipe.Expire(ctx, rankStateKey, ttl)
	}
	if len(changedUsers) > 0 {
		userStateKey := t.getKey("user_state")
		params := make([]interface{}, 0, len(changedUsers)*2)
		for k, v := range changedUsers {
			data, _ := json.Marshal(v)
			params = append(params, k, string(data))
		}
		pipe.HSet(ctx, userStateKey, params...)
		pipe.Expire(ctx, userStateKey, ttl)
	}

	if _, err := pipe.Exec(ctx); err != nil {
		t.logger.Warnf("Failed to save state to Redis: %v", err)
	}
}

func (t *EventTrackerBase) checkEventEndedFlag(ctx context.Context) bool {
	key := t.getKey("ended")
	val, err := t.redisClient.Get(ctx, key).Result()
	if err == nil && val == "true" {
		t.logger.Infof("Event ended flag found in Redis, skipping initialization")
		t.isEventEnded = true
		return true
	}
	return false
}

func (t *EventTrackerBase) setEventEndedFlag(ctx context.Context) {
	key := t.getKey("ended")
	t.redisClient.Set(ctx, key, "true", 24*time.Hour*30)
}

func (t *EventTrackerBase) Init(ctx context.Context) error {
	t.logger.Infof("Initializing %s %d event tracker...", t.server, t.eventID)

	if t.checkEventEndedFlag(ctx) {
		return nil
	}

	t.loadStateFromRedis(ctx)

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

func (t *EventTrackerBase) extractWorldBloomRankings(top100 *model.Top100RankingResponse, border *model.BorderRankingResponse) map[int][]model.PlayerRankingSchema {
	result := make(map[int][]model.PlayerRankingSchema)

	if top100.UserWorldBloomChapterRankings == nil || len(top100.UserWorldBloomChapterRankings) == 0 {
		return result
	}
	for _, chapter := range top100.UserWorldBloomChapterRankings {
		charID, rankings := t.processWorldBloomChapter(chapter)
		if charID != nil && len(rankings) > 0 {
			var borderRankings []model.PlayerRankingSchema
			if border != nil && border.UserWorldBloomChapterRankingBorders != nil {
				borderRankings = t.extractWorldBloomBorderRankings(border.UserWorldBloomChapterRankingBorders, *charID)
			}
			result[*charID] = t.mergeWorldBloomRankingsForCharacter(rankings, borderRankings)
		}
	}

	return result
}

func (t *EventTrackerBase) mergeWorldBloomRankingsForCharacter(top100Rankings []model.PlayerRankingSchema, borderRankings []model.PlayerRankingSchema) []model.PlayerRankingSchema {
	if len(borderRankings) == 0 {
		return top100Rankings
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

	return result
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

	if chapter.IsWorldBloomChapterAggregate != nil && *chapter.IsWorldBloomChapterAggregate {
		return nil, nil
	}
	chapterEnded := t.isWorldBloomChapterEnded[charID]
	shouldTrack := false
	if status.ChapterStatus == model.SekaiEventStatusOngoing {
		shouldTrack = true
	} else if status.ChapterStatus == model.SekaiEventStatusEnded && !chapterEnded {
		shouldTrack = true
		t.logger.Infof("Recording final rankings for world bloom chapter (character ID: %d)", charID)
	}

	if shouldTrack && chapter.Rankings != nil && len(chapter.Rankings) > 0 {
		return &charID, chapter.Rankings
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

	var wlRankingsMap map[int][]model.PlayerRankingSchema
	if t.eventType == model.SekaiEventTypeWorldBloom {
		wlRankingsMap = t.extractWorldBloomRankings(top100, border)
	}

	mainCacheKey := fmt.Sprintf("%s-event-%d-main-border", t.server, t.eventID)
	rankings, err := t.mergeRankings(ctx, mainTop100Rankings, mainBorderRankings, borderHash, mainCacheKey)
	if err != nil {
		return nil, fmt.Errorf("failed to merge main rankings: %w", err)
	}

	return &HandledRankingData{
		RecordTime:         currentTime,
		Rankings:           rankings,
		WorldBloomRankings: wlRankingsMap,
	}, nil
}

func (t *EventTrackerBase) diffRankBased(
	data []model.PlayerRankingSchema,
	changedRanks map[int]model.RankState,
) []*model.PlayerRankingSchema {
	var result []*model.PlayerRankingSchema
	for i := range data {
		r := &data[i]
		if r.Rank == nil || r.Score == nil || r.UserID == nil {
			continue
		}

		rank := *r.Rank
		score := *r.Score
		userID := fmt.Sprintf("%d", *r.UserID)

		prev, exists := t.prevRankState[rank]
		if !exists || prev.Score != score || prev.UserID != userID {
			result = append(result, r)
			newState := model.RankState{UserID: userID, Score: score}
			t.prevRankState[rank] = newState
			changedRanks[rank] = newState
		}
	}
	return result
}

// Specific player tracking logic has been removed.

func (t *EventTrackerBase) buildEventRecords(
	recordTime int64,
	rankBasedRows []*model.PlayerRankingSchema,
) []*model.PlayerEventRankingRecordSchema {
	uniqueRecords := make(map[string]*model.PlayerEventRankingRecordSchema)

	addRecord := func(r *model.PlayerRankingSchema) {
		userID := fmt.Sprintf("%d", *r.UserID)
		if _, exists := uniqueRecords[userID]; exists {
			return
		}
		uniqueRecords[userID] = &model.PlayerEventRankingRecordSchema{
			Timestamp:      recordTime,
			UserID:         userID,
			Name:           *r.Name,
			Score:          *r.Score,
			Rank:           *r.Rank,
			CheerfulTeamID: t.extractCheerfulTeamID(r),
		}
	}

	for _, r := range rankBasedRows {
		addRecord(r)
	}

	result := make([]*model.PlayerEventRankingRecordSchema, 0, len(uniqueRecords))
	for _, r := range uniqueRecords {
		result = append(result, r)
	}
	return result
}

func (t *EventTrackerBase) getFilterFunc(currentTimeMinute string) func(*model.PlayerRankingSchema) bool {
	if currentTimeMinute != t.lastUpdateTime {
		t.lastUpdateTime = currentTimeMinute
		return func(r *model.PlayerRankingSchema) bool { return true }
	}
	return func(r *model.PlayerRankingSchema) bool {
		return true
	}
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
	if len(data.WorldBloomRankings) == 0 {
		return nil
	}

	wlRows := make([]*model.PlayerWorldBloomRankingRecordSchema, 0)

	for characterID, rankings := range data.WorldBloomRankings {
		for i := range rankings {
			r := &rankings[i]
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
				CharacterID: characterID,
			})
		}
	}

	return wlRows
}

func (t *EventTrackerBase) RecordRankingData(ctx context.Context, isOnlyRecordWorldBloom bool) error {
	if t.IsEventEnded() {
		t.logger.Infof("Detected event ended, skipping ranking data recording.")
		return nil
	}

	currentTime := time.Now().Unix()
	data, err := t.handleRankingData(ctx)
	if err != nil {
		t.logger.Warnf("API error, writing heartbeat with status=1: %v", err)
		if heartbeatErr := gorm.WriteHeartbeat(ctx, t.dbEngine, t.server, t.eventID, currentTime, 1); heartbeatErr != nil {
			t.logger.Errorf("Failed to write heartbeat on API error: %v", heartbeatErr)
			return fmt.Errorf("API error: %w (heartbeat write also failed: %v)", err, heartbeatErr)
		}
		return err
	}
	if data == nil {
		return nil
	}

	t.logger.Infof("%s server tracker started inserting ranking data...", t.server)

	batchFunctionCalled := false

	changedRanks := make(map[int]model.RankState)
	changedUsers := make(map[string]model.PlayerState) // Kept for struct compatibility

	if !isOnlyRecordWorldBloom && len(data.Rankings) > 0 {
		rankBasedDiffs := t.diffRankBased(data.Rankings, changedRanks)
		eventRows := t.buildEventRecords(data.RecordTime, rankBasedDiffs)

		if len(eventRows) > 0 {
			if err := gorm.BatchInsertEventRankings(ctx, t.dbEngine, t.server, t.eventID, eventRows, nil); err != nil {
				return fmt.Errorf("failed to insert event rankings: %w", err)
			}
			batchFunctionCalled = true
		}
	}

	wlRows := t.buildWorldBloomRows(data)
	if len(wlRows) > 0 {
		if err := gorm.BatchInsertWorldBloomRankings(ctx, t.dbEngine, t.server, t.eventID, wlRows, t.prevWorldBloomState); err != nil {
			return fmt.Errorf("failed to insert world bloom rankings: %w", err)
		}
		batchFunctionCalled = true
	}

	if !batchFunctionCalled {
		if heartbeatErr := gorm.WriteHeartbeat(ctx, t.dbEngine, t.server, t.eventID, currentTime, 0); heartbeatErr != nil {
			t.logger.Errorf("Failed to write heartbeat with no input data: %v", heartbeatErr)
			return fmt.Errorf("failed to write heartbeat: %w", heartbeatErr)
		}
	}

	t.saveStateToRedis(ctx, changedRanks, changedUsers)
	t.logger.Infof("%s server tracker finished inserting ranking data.", t.server)
	return nil
}

func (t *EventTrackerBase) SetEventEnded(ended bool) {
	t.isEventEnded = ended
	if ended {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		t.setEventEndedFlag(ctx)
	}
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
