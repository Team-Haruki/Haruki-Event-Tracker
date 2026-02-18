package model

type PlayerEventRankingRecordSchema struct {
	Timestamp      int64
	UserID         string
	Name           string
	Score          int
	Rank           int
	CheerfulTeamID *int
}

type PlayerWorldBloomRankingRecordSchema struct {
	PlayerEventRankingRecordSchema
	CharacterID int
}

type PlayerState struct {
	Score int `json:"s"`
	Rank  int `json:"r"`
}

type RankState struct {
	UserID string `json:"u"`
	Score  int    `json:"s"`
}

type WorldBloomKey struct {
	UserIDKey   int
	CharacterID int
}

type SecondLevelEventTrackType string

const (
	SecondLevelEventTrackTypeRange    SecondLevelEventTrackType = "range"
	SecondLevelEventTrackTypeSpecific SecondLevelEventTrackType = "specific"
)
