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

type TimeUnit struct {
	Second string
	Minute string
	Hour   string
	Day    string
}

type EventDataParser struct {
	server         model.SekaiServerRegion
	masterDir      string
	cachedData     map[string]interface{}
	cachedDataHash map[string]string
	cacheMutex     sync.RWMutex
}

func NewEventDataParser(server model.SekaiServerRegion, masterDir string) *EventDataParser {
	return &EventDataParser{
		server:         server,
		masterDir:      masterDir,
		cachedData:     make(map[string]interface{}),
		cachedDataHash: make(map[string]string),
	}
}

func (p *EventDataParser) LoadData(path string) (interface{}, error) {
	p.cacheMutex.Lock()
	defer p.cacheMutex.Unlock()
	if cached, ok := p.cachedData[path]; ok {
		rawData, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("failed to read file: %w", err)
		}
		currentHash := ComputeHash(rawData)
		if p.cachedDataHash[path] == currentHash {
			return cached, nil
		}
		var parsed interface{}
		if err := sonic.Unmarshal(rawData, &parsed); err != nil {
			return nil, fmt.Errorf("failed to unmarshal JSON: %w", err)
		}

		p.cachedData[path] = parsed
		p.cachedDataHash[path] = currentHash
		return parsed, nil
	}
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

func (p *EventDataParser) LoadEventData() ([]model.Event, error) {
	path := filepath.Join(p.masterDir, "events.json")
	rawData, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read file: %w", err)
	}
	var events []model.Event
	if err := sonic.Unmarshal(rawData, &events); err != nil {
		return nil, fmt.Errorf("failed to unmarshal JSON: %w", err)
	}
	return events, nil
}

func (p *EventDataParser) LoadWorldBloomChapterData() ([]model.WorldBloom, error) {
	path := filepath.Join(p.masterDir, "worldBlooms.json")
	rawData, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read file: %w", err)
	}

	var worldBlooms []model.WorldBloom
	if err := sonic.Unmarshal(rawData, &worldBlooms); err != nil {
		return nil, fmt.Errorf("failed to unmarshal JSON: %w", err)
	}

	return worldBlooms, nil
}

func (p *EventDataParser) GetWorldBloomCharacterStatuses(eventID int) (map[int]model.WorldBloomChapterStatus, error) {
	data, err := p.LoadWorldBloomChapterData()
	if err != nil {
		return nil, err
	}
	now := time.Now().UnixMilli()
	result := make(map[int]model.WorldBloomChapterStatus)
	for _, chapter := range data {
		if chapter.EventID != eventID {
			continue
		}
		if chapter.WorldBloomChapterType == model.SekaiWorldBloomTypeFinale {
			continue
		}
		characterID := chapter.GameCharacterID
		var chapterStatus model.SekaiEventStatus
		if chapter.ChapterEndAt <= now {
			chapterStatus = model.SekaiEventStatusEnded
		} else if chapter.AggregateAt < now && now < chapter.ChapterEndAt {
			chapterStatus = model.SekaiEventStatusAggregating
		} else if chapter.ChapterStartAt < now && now < chapter.AggregateAt {
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

func (p *EventDataParser) GetCurrentEventStatus() (*model.EventStatus, error) {
	data, err := p.LoadEventData()
	if err != nil {
		return nil, err
	}
	now := time.Now().UnixMilli()
	for _, event := range data {
		if !(event.StartAt < now && now < event.ClosedAt) {
			continue
		}
		var status model.SekaiEventStatus
		var remain string
		if event.StartAt < now && now < event.AggregateAt {
			status = model.SekaiEventStatusOngoing
			remainTime := (event.AggregateAt - now) / 1000
			remain = EventTimeRemain(float64(remainTime), true, p.server)
		} else if event.AggregateAt < now && now < event.AggregateAt+600000 {
			status = model.SekaiEventStatusAggregating
		} else {
			status = model.SekaiEventStatusEnded
		}
		var chapterStatuses map[int]model.WorldBloomChapterStatus
		if event.EventType == model.SekaiEventTypeWorldBloom {
			chapterStatuses, err = p.GetWorldBloomCharacterStatuses(event.ID)
			if err != nil {
				return nil, err
			}
		}
		return &model.EventStatus{
			Server:          p.server,
			EventID:         event.ID,
			EventType:       event.EventType,
			EventStatus:     status,
			Remain:          remain,
			AssetbundleName: event.AssetbundleName,
			ChapterStatuses: chapterStatuses,
			Detail:          event,
		}, nil
	}
	return nil, nil
}

func ComputeHash(data []byte) string {
	hash := sha256.Sum256(data)
	return hex.EncodeToString(hash[:])
}

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
