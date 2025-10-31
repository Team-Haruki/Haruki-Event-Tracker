package model

// RecordedRankingSchema represents recorded ranking data
type RecordedRankingSchema struct {
	Timestamp int64  `json:"timestamp"`
	UserID    string `json:"user_id"`
	Score     int    `json:"score"`
	Rank      int    `json:"rank"`
}

// RecordedWorldBloomRankingSchema represents recorded world bloom ranking data
type RecordedWorldBloomRankingSchema struct {
	RecordedRankingSchema
	CharacterID *int `json:"character_id,omitempty"`
}

// RecordedUserNameSchema represents recorded user name
type RecordedUserNameSchema struct {
	UserID         string `json:"user_id"`
	Name           string `json:"name"`
	CheerfulTeamID *int   `json:"cheerful_team_id,omitempty"`
}

// UserLatestRankingQueryResponseSchema represents user latest ranking query response
type UserLatestRankingQueryResponseSchema struct {
	RankData interface{}             `json:"rank_data,omitempty"` // Can be RecordedRankingSchema or RecordedWorldBloomRankingSchema
	UserData *RecordedUserNameSchema `json:"user_data,omitempty"`
}

// UserAllRankingDataQueryResponseSchema represents user all ranking data query response
type UserAllRankingDataQueryResponseSchema struct {
	RankData []interface{}           `json:"rank_data,omitempty"` // List of RecordedRankingSchema or RecordedWorldBloomRankingSchema
	UserData *RecordedUserNameSchema `json:"user_data,omitempty"`
}

// RankingLineScoreSchema represents ranking line score
type RankingLineScoreSchema struct {
	Rank      int   `json:"rank"`
	Score     int   `json:"score"`
	Timestamp int64 `json:"timestamp"`
}

// RankingScoreGrowthSchema represents ranking score growth
type RankingScoreGrowthSchema struct {
	Rank             int    `json:"rank"`
	TimestampLatest  int64  `json:"timestamp_latest"`
	ScoreLatest      int    `json:"score_latest"`
	TimestampEarlier *int64 `json:"timestamp_earlier,omitempty"`
	ScoreEarlier     *int   `json:"score_earlier,omitempty"`
	Growth           *int   `json:"growth,omitempty"`
}
