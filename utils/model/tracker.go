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
	Score int
	Rank  int
}

type SecondLevelEventTrackType string

const (
	SecondLevelEventTrackTypeRange    SecondLevelEventTrackType = "range"
	SecondLevelEventTrackTypeSpecific SecondLevelEventTrackType = "specific"
)
