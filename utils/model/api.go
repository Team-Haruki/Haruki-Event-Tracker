package model

type RecordedRankingSchema struct {
	Timestamp int64  `json:"timestamp"`
	UserID    string `json:"userId"`
	Score     int    `json:"score"`
	Rank      int    `json:"rank"`
}

type RecordedWorldBloomRankingSchema struct {
	RecordedRankingSchema
	CharacterID *int `json:"characterId,omitempty"`
}

type RecordedUserNameSchema struct {
	UserID         string `json:"userId"`
	Name           string `json:"name"`
	CheerfulTeamID *int   `json:"cheerfulTeamId,omitempty"`
}

type UserLatestRankingQueryResponseSchema struct {
	RankData interface{}             `json:"rankData,omitempty"`
	UserData *RecordedUserNameSchema `json:"userData,omitempty"`
}

type UserAllRankingDataQueryResponseSchema struct {
	RankData []interface{}           `json:"rankData,omitempty"`
	UserData *RecordedUserNameSchema `json:"userData,omitempty"`
}

type RankingLineScoreSchema struct {
	Rank      int   `json:"rank"`
	Score     int   `json:"score"`
	Timestamp int64 `json:"timestamp"`
}

type RankingScoreGrowthSchema struct {
	Rank             int    `json:"rank"`
	TimestampLatest  int64  `json:"timestampLatest"`
	ScoreLatest      int    `json:"scoreLatest"`
	TimestampEarlier *int64 `json:"timestampEarlier,omitempty"`
	ScoreEarlier     *int   `json:"scoreEarlier,omitempty"`
	TimeDiff         *int64 `json:"timeDiff,omitempty"`
	Growth           *int   `json:"growth,omitempty"`
}
