package model

// RecordPlayerRankingSchemaBase represents base player ranking record
type RecordPlayerRankingSchemaBase struct {
	Timestamp int64 `json:"timestamp"`
	UserID    int   `json:"user_id"`
	Score     int   `json:"score"`
	Rank      int   `json:"rank"`
}

// RecordChapterPlayerRankingSchema represents chapter player ranking record
type RecordChapterPlayerRankingSchema struct {
	RecordPlayerRankingSchemaBase
	CharacterID int `json:"character_id"`
}

// RecordPlayerNameSchema represents player name record
type RecordPlayerNameSchema struct {
	UserID         int    `json:"user_id"`
	Name           string `json:"name"`
	CheerfulTeamID *int   `json:"cheerful_team_id,omitempty"`
}

// HandledRankingDataSchema represents handled ranking data
type HandledRankingDataSchema struct {
	RecordTime         int64                 `json:"record_time"`
	Rankings           []PlayerRankingSchema `json:"rankings"`
	CharacterID        *int                  `json:"character_id,omitempty"`
	WorldBloomRankings []PlayerRankingSchema `json:"world_bloom_rankings,omitempty"`
}
