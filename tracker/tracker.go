package tracker

import (
	"context"
	"fmt"
	"haruki-tracker/utils/gorm"
	"haruki-tracker/utils/logger"
	"haruki-tracker/utils/model"
	"strings"
	"time"

	"github.com/redis/go-redis/v9"
)

type HarukiEventTracker struct {
	server     model.SekaiServerRegion
	sekaiAPI   HarukiSekaiAPIClient
	redis      *redis.Client
	dbEngine   *gorm.DatabaseEngine
	dataParser *EventDataParser
	tracker    *EventTrackerBase
	logger     *logger.Logger
}

func NewHarukiEventTracker(server model.SekaiServerRegion, apiClient *HarukiSekaiAPIClient, redisClient *redis.Client, dbEngine *gorm.DatabaseEngine, masterDir string) *HarukiEventTracker {
	return &HarukiEventTracker{
		server:     server,
		sekaiAPI:   *apiClient,
		redis:      redisClient,
		dbEngine:   dbEngine,
		dataParser: NewEventDataParser(server, masterDir),
		logger:     logger.NewLogger(fmt.Sprintf("HarukiEventTracker%sDaemon", strings.ToUpper(string(server))), "INFO", nil),
	}
}

func (t *HarukiEventTracker) Init() error {
	event, err := t.dataParser.GetCurrentEventStatus()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if err != nil {
		return err
	}
	if event == nil {
		return fmt.Errorf("no active event found for server %s", t.server)
	}
	isEventEnded := event.EventStatus == model.SekaiEventStatusEnded
	t.tracker = NewEventTrackerBase(t.server, event.EventID, event.EventType, isEventEnded, t.dbEngine, t.redis, &t.sekaiAPI, event.ChapterStatuses)
	err = t.tracker.Init(ctx)
	if err != nil {
		t.logger.Errorf("Tracker Init Error: %s", err.Error())
		return fmt.Errorf("failed to initialize event tracker for server %s: %w", t.server, err)
	}
	return nil
}

func (t *HarukiEventTracker) handleEventEnded(ctx context.Context, event *model.EventStatus) bool {
	if event.EventStatus != model.SekaiEventStatusEnded || t.tracker.IsEventEnded() {
		return false
	}

	t.logger.Infof("Event %d has ended, finalizing tracking...", event.EventID)
	err := t.tracker.RecordRankingData(ctx, false)
	if err != nil {
		t.logger.Errorf("Failed to record final ranking data for event %d: %v", event.EventID, err)
	}
	t.tracker.SetEventEnded(true)
	return true
}

func (t *HarukiEventTracker) handleWorldBloomChapter(ctx context.Context, event *model.EventStatus, characterID int, detail model.WorldBloomChapterStatus) bool {
	switch detail.ChapterStatus {
	case model.SekaiEventStatusNotStarted:
		return false
	case model.SekaiEventStatusAggregating:
		t.logger.Infof("World bloom event %d chapter %d is in aggregating, skipping tracking...", event.EventID, characterID)
		return false
	case model.SekaiEventStatusEnded:
		if t.tracker.IsWorldBloomChapterEnded(characterID) {
			return false
		}
		t.logger.Infof("World bloom event %d chapter %d has ended, finalizing tracking...", event.EventID, characterID)
		if err := t.tracker.RecordRankingData(ctx, true); err != nil {
			t.logger.Errorf("Failed to record world bloom final ranking data for event %d chapter %d: %v", event.EventID, characterID, err)
		}
		t.tracker.SetWorldBloomChapterEnded(characterID, true)
		return true
	default:
		return false
	}
}

func (t *HarukiEventTracker) handleWorldBloom(ctx context.Context, event *model.EventStatus) {
	if !worldBloomStatusesEqual(t.tracker.GetWorldBloomChapterStatus(), event.ChapterStatuses) {
		t.tracker.SetWorldBloomChapterStatus(event.ChapterStatuses)
	}

	for characterID, detail := range event.ChapterStatuses {
		if t.handleWorldBloomChapter(ctx, event, characterID, detail) {
			break
		}
	}
}

func (t *HarukiEventTracker) handleTrackerMatch(ctx context.Context, event *model.EventStatus) bool {
	if t.tracker.IsEventEnded() {
		t.logger.Infof("Event %d has already ended, skipping tracking...", event.EventID)
		return true
	}

	if event.EventStatus == model.SekaiEventStatusAggregating {
		t.logger.Infof("Event %d is in aggregating, skipping tracking...", event.EventID)
		return true
	}

	if t.handleEventEnded(ctx, event) {
		return true
	}

	if event.EventType == model.SekaiEventTypeWorldBloom {
		t.handleWorldBloom(ctx, event)
	}

	return false
}

func (t *HarukiEventTracker) TrackRankingData() {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	event, err := t.dataParser.GetCurrentEventStatus()
	if err != nil {
		t.logger.Errorf("Failed to get current event status: %v", err)
		return
	}

	if event == nil {
		t.logger.Infof("No active event found, skipping tracking...")
		return
	}

	if t.tracker == nil {
		t.logger.Infof("Initializing tracker for event %d...", event.EventID)
		if err = t.Init(); err != nil {
			t.logger.Errorf("Failed to initialize tracker: %v", err)
			return
		}
	} else if t.tracker.GetEventID() < event.EventID {
		t.logger.Infof("Tracker daemon detected new event %d, switching tracker...", event.EventID)
		if err = t.Init(); err != nil {
			t.logger.Errorf("Failed to initialize tracker for new event %d: %v", event.EventID, err)
			return
		}
	} else if t.tracker.GetEventID() == event.EventID {
		if t.handleTrackerMatch(ctx, event) {
			return
		}
	}

	t.logger.Infof("Tracker start tracking data for event %d...", event.EventID)
	err = t.tracker.RecordRankingData(ctx, false)
	if err != nil {
		t.logger.Errorf("Failed to record ranking data for event %d: %v", event.EventID, err)
		return
	}
	t.logger.Infof("Tracker finished tracking data for event %d.", event.EventID)
}
