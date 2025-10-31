package tracker

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"

	"haruki-tracker/utils/model"

	"github.com/bytedance/sonic"
)

// EventDataParser handles parsing and caching of event data
type EventDataParser struct {
	server         model.SekaiServerRegion
	masterDir      string
	cachedData     map[string]interface{}
	cachedDataHash map[string]string
	cacheMutex     sync.RWMutex
}

// NewEventDataParser creates a new EventDataParser instance
func NewEventDataParser(server model.SekaiServerRegion, masterDir string) *EventDataParser {
	return &EventDataParser{
		server:         server,
		masterDir:      masterDir,
		cachedData:     make(map[string]interface{}),
		cachedDataHash: make(map[string]string),
	}
}

// ComputeHash computes SHA256 hash of data
func ComputeHash(data []byte) string {
	hash := sha256.Sum256(data)
	return hex.EncodeToString(hash[:])
}

// TimeUnit represents time translation units
type TimeUnit struct {
	Second string
	Minute string
	Hour   string
	Day    string
}

// GetTimeTranslations returns time unit translations for different servers
func GetTimeTranslations(server model.SekaiServerRegion) TimeUnit {
	translations := map[model.SekaiServerRegion]TimeUnit{
		model.SekaiServerRegionJP: {"秒", "分", "小时", "天"},
		model.SekaiServerRegionCN: {"秒", "分", "小时", "天"},
		model.SekaiServerRegionTW: {"秒", "分", "小時", "天"},
		model.SekaiServerRegionEN: {"s", "m", "h", "d"},
		model.SekaiServerRegionKR: {"초", "분", "시간", "일"},
	}

	if t, ok := translations[server]; ok {
		return t
	}
	return translations[model.SekaiServerRegionJP]
}

// EventTimeRemain formats remaining time in human-readable format
func EventTimeRemain(remainTime float64, showSeconds bool, server model.SekaiServerRegion) string {
	t := GetTimeTranslations(server)
	remainTimeInt := int(remainTime)

	if remainTime < 60 {
		if showSeconds {
			return fmt.Sprintf("%d%s", remainTimeInt, t.Second)
		}
		return fmt.Sprintf("0%s", t.Minute)
	} else if remainTime < 60*60 {
		minutes := remainTimeInt / 60
		seconds := remainTimeInt % 60
		if showSeconds {
			return fmt.Sprintf("%d%s%d%s", minutes, t.Minute, seconds, t.Second)
		}
		return fmt.Sprintf("%d%s", minutes, t.Minute)
	} else if remainTime < 60*60*24 {
		hours := remainTimeInt / 3600
		remain := remainTimeInt - 3600*hours
		minutes := remain / 60
		seconds := remain % 60
		if showSeconds {
			return fmt.Sprintf("%d%s%d%s%d%s", hours, t.Hour, minutes, t.Minute, seconds, t.Second)
		}
		return fmt.Sprintf("%d%s%d%s", hours, t.Hour, minutes, t.Minute)
	} else {
		days := remainTimeInt / (3600 * 24)
		remain := float64(remainTimeInt - 3600*24*days)
		return fmt.Sprintf("%d%s%s", days, t.Day, EventTimeRemain(remain, showSeconds, server))
	}
}

// LoadData loads and caches JSON data from a file
func (p *EventDataParser) LoadData(path string) (interface{}, error) {
	p.cacheMutex.Lock()
	defer p.cacheMutex.Unlock()

	// Check if cached
	if cached, ok := p.cachedData[path]; ok {
		// Verify hash
		rawData, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("failed to read file: %w", err)
		}

		currentHash := ComputeHash(rawData)
		if p.cachedDataHash[path] == currentHash {
			return cached, nil
		}

		// Hash changed, reload
		var parsed interface{}
		if err := sonic.Unmarshal(rawData, &parsed); err != nil {
			return nil, fmt.Errorf("failed to unmarshal JSON: %w", err)
		}

		p.cachedData[path] = parsed
		p.cachedDataHash[path] = currentHash
		return parsed, nil
	}

	// Load for the first time
	rawData, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read file: %w", err)
	}

	var parsed interface{}
	if err := sonic.Unmarshal(rawData, &parsed); err != nil {
		return nil, fmt.Errorf("failed to unmarshal JSON: %w", err)
	}

	p.cachedData[path] = parsed
	p.cachedDataHash[path] = ComputeHash(rawData)
	return parsed, nil
}

// LoadEventData loads events.json data
func (p *EventDataParser) LoadEventData() ([]map[string]interface{}, error) {
	path := filepath.Join(p.masterDir, "events.json")
	data, err := p.LoadData(path)
	if err != nil {
		return nil, err
	}

	if arr, ok := data.([]interface{}); ok {
		result := make([]map[string]interface{}, 0, len(arr))
		for _, item := range arr {
			if m, ok := item.(map[string]interface{}); ok {
				result = append(result, m)
			}
		}
		return result, nil
	}

	return nil, fmt.Errorf("unexpected data format")
}

// LoadWorldBloomChapterData loads worldBlooms.json data
func (p *EventDataParser) LoadWorldBloomChapterData() ([]map[string]interface{}, error) {
	path := filepath.Join(p.masterDir, "worldBlooms.json")
	data, err := p.LoadData(path)
	if err != nil {
		return nil, err
	}

	if arr, ok := data.([]interface{}); ok {
		result := make([]map[string]interface{}, 0, len(arr))
		for _, item := range arr {
			if m, ok := item.(map[string]interface{}); ok {
				result = append(result, m)
			}
		}
		return result, nil
	}

	return nil, fmt.Errorf("unexpected data format")
}

// GetWorldBloomCharacterStatuses retrieves World Bloom chapter statuses
func (p *EventDataParser) GetWorldBloomCharacterStatuses(eventID int) (map[int]model.WorldBloomChapterStatus, error) {
	data, err := p.LoadWorldBloomChapterData()
	if err != nil {
		return nil, err
	}

	now := time.Now().UnixMilli()
	result := make(map[int]model.WorldBloomChapterStatus)

	for _, chapter := range data {
		eventIDVal, ok1 := chapter["eventId"].(float64)
		characterIDVal, ok2 := chapter["characterId"].(float64)
		chapterStartAt, ok3 := chapter["chapterStartAt"].(float64)
		aggregateAt, ok4 := chapter["aggregateAt"].(float64)
		chapterEndAt, ok5 := chapter["chapterEndAt"].(float64)

		if !ok1 || !ok2 || !ok3 || !ok4 || !ok5 {
			continue
		}

		if int(eventIDVal) != eventID {
			continue
		}

		characterID := int(characterIDVal)
		var chapterStatus model.SekaiEventStatus

		if int64(chapterEndAt) <= now {
			chapterStatus = model.SekaiEventStatusEnded
		} else if int64(aggregateAt) < now && now < int64(chapterEndAt) {
			chapterStatus = model.SekaiEventStatusAggregating
		} else if int64(chapterStartAt) < now && now < int64(aggregateAt) {
			chapterStatus = model.SekaiEventStatusOngoing
		} else {
			chapterStatus = model.SekaiEventStatusNotStarted
		}

		result[characterID] = model.WorldBloomChapterStatus{
			Server:        p.server,
			EventID:       eventID,
			CharacterID:   characterID,
			ChapterStatus: chapterStatus,
		}
	}

	return result, nil
}

// GetCurrentEventStatus retrieves the current ongoing event status
func (p *EventDataParser) GetCurrentEventStatus() (*model.EventStatus, error) {
	data, err := p.LoadEventData()
	if err != nil {
		return nil, err
	}

	now := time.Now().UnixMilli()

	for _, event := range data {
		startAt, ok1 := event["startAt"].(float64)
		endAt, ok2 := event["closedAt"].(float64)
		assetbundleName, ok3 := event["assetbundleName"].(string)

		if !ok1 || !ok2 || !ok3 {
			continue
		}

		if !(int64(startAt) < now && now < int64(endAt)) {
			continue
		}

		eventIDVal, ok := event["id"].(float64)
		if !ok {
			continue
		}
		eventID := int(eventIDVal)

		eventTypeStr, ok := event["eventType"].(string)
		if !ok {
			continue
		}
		eventType := model.SekaiEventType(eventTypeStr)

		aggregateAt, ok := event["aggregateAt"].(float64)
		if !ok {
			continue
		}

		var status model.SekaiEventStatus
		var remain string

		if int64(startAt) < now && now < int64(aggregateAt) {
			status = model.SekaiEventStatusOngoing
			remainTime := (int64(aggregateAt) - now) / 1000
			remain = EventTimeRemain(float64(remainTime), true, p.server)
		} else if int64(aggregateAt) < now && now < int64(aggregateAt)+600000 {
			status = model.SekaiEventStatusAggregating
		} else {
			status = model.SekaiEventStatusEnded
		}

		var chapterStatuses map[int]model.WorldBloomChapterStatus
		if eventType == model.SekaiEventTypeWorldBloom {
			chapterStatuses, err = p.GetWorldBloomCharacterStatuses(eventID)
			if err != nil {
				return nil, err
			}
		}

		// Convert event detail to map[string]interface{}
		detail := make(map[string]interface{})
		for k, v := range event {
			detail[k] = v
		}

		return &model.EventStatus{
			Server:          p.server,
			EventID:         eventID,
			EventType:       eventType,
			EventStatus:     status,
			Remain:          remain,
			AssetbundleName: assetbundleName,
			ChapterStatuses: chapterStatuses,
			Detail:          detail,
		}, nil
	}

	return nil, nil // No current event
}
