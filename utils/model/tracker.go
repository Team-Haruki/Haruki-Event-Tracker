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
