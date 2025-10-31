package model

// WorldBloomChapterStatus represents world bloom chapter status
type WorldBloomChapterStatus struct {
	Server        SekaiServerRegion `json:"server"`
	EventID       int               `json:"event_id"`
	CharacterID   int               `json:"character_id"`
	ChapterStatus SekaiEventStatus  `json:"chapter_status"`
}

// EventStatus represents event status
type EventStatus struct {
	Server          SekaiServerRegion               `json:"server"`
	EventID         int                             `json:"event_id"`
	EventType       SekaiEventType                  `json:"event_type"`
	EventStatus     SekaiEventStatus                `json:"event_status"`
	Remain          string                          `json:"remain"`
	AssetbundleName string                          `json:"assetbundle_name"`
	ChapterStatuses map[int]WorldBloomChapterStatus `json:"chapter_statuses,omitempty"`
	Detail          map[string]interface{}          `json:"detail"`
}
