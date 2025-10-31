package model

// UserCard represents user card information
type UserCard struct {
	CardID                *int    `json:"cardId,omitempty"`
	DefaultImage          *string `json:"defaultImage,omitempty"` // "original" or "special_training"
	Level                 *int    `json:"level,omitempty"`
	MasterRank            *int    `json:"masterRank,omitempty"`
	SpecialTrainingStatus *string `json:"specialTrainingStatus,omitempty"` // "not_doing" or "done"
}

// UserCheerfulCarnival represents user cheerful carnival information
type UserCheerfulCarnival struct {
	CheerfulCarnivalTeamID *int   `json:"cheerfulCarnivalTeamId,omitempty"`
	EventID                *int   `json:"eventId,omitempty"`
	RegisterAt             *int64 `json:"registerAt,omitempty"`
	TeamChangeCount        *int   `json:"teamChangeCount,omitempty"`
}

// UserProfile represents user profile information
type UserProfile struct {
	ProfileImageType *string `json:"profileImageType,omitempty"`
	TwitterID        *string `json:"twitterId,omitempty"`
	UserID           *int    `json:"userId,omitempty"`
	Word             *string `json:"word,omitempty"`
}

// UserProfileHonor represents user profile honor information
type UserProfileHonor struct {
	BondsHonorViewType *string `json:"bondsHonorViewType,omitempty"` // "none", "normal", "reverse", etc.
	BondsHonorWordID   *int    `json:"bondsHonorWordId,omitempty"`
	HonorID            *int    `json:"honorId,omitempty"`
	HonorLevel         *int    `json:"honorLevel,omitempty"`
	ProfileHonorType   *string `json:"profileHonorType,omitempty"` // "normal" or "bonds"
	Seq                *int    `json:"seq,omitempty"`
}

// PlayerRankingSchema represents player ranking data
type PlayerRankingSchema struct {
	IsOwn                *bool                 `json:"isOwn,omitempty"`
	Name                 *string               `json:"name,omitempty"`
	Rank                 *int                  `json:"rank,omitempty"`
	Score                *int                  `json:"score,omitempty"`
	UserCard             *UserCard             `json:"userCard,omitempty"`
	UserCheerfulCarnival *UserCheerfulCarnival `json:"userCheerfulCarnival,omitempty"`
	UserHonorMissions    []interface{}         `json:"userHonorMissions,omitempty"`
	UserID               *int                  `json:"userId,omitempty"`
	UserProfile          *UserProfile          `json:"userProfile,omitempty"`
	UserProfileHonors    []UserProfileHonor    `json:"userProfileHonors,omitempty"`
}

// UserWorldBloomChapterRankingBase represents base world bloom chapter ranking
type UserWorldBloomChapterRankingBase struct {
	EventID                      *int  `json:"eventId,omitempty"`
	GameCharacterID              *int  `json:"gameCharacterId,omitempty"`
	IsWorldBloomChapterAggregate *bool `json:"isWorldBloomChapterAggregate,omitempty"`
}

// UserWorldBloomChapterRanking represents world bloom chapter ranking
type UserWorldBloomChapterRanking struct {
	UserWorldBloomChapterRankingBase
	Rankings []PlayerRankingSchema `json:"rankings,omitempty"`
}

// UserWorldBloomChapterRankingBorder represents world bloom chapter ranking border
type UserWorldBloomChapterRankingBorder struct {
	UserWorldBloomChapterRankingBase
	BorderRankings []PlayerRankingSchema `json:"borderRankings,omitempty"`
}

// Top100RankingResponse represents top 100 ranking response
type Top100RankingResponse struct {
	IsEventAggregate              *bool                          `json:"isEventAggregate,omitempty"`
	Rankings                      []PlayerRankingSchema          `json:"rankings,omitempty"`
	UserRankingStatus             string                         `json:"userRankingStatus"` // "normal", "event_aggregate", "world_aggregate"
	UserWorldBloomChapterRankings []UserWorldBloomChapterRanking `json:"userWorldBloomChapterRankings,omitempty"`
}

// BorderRankingResponse represents border ranking response
type BorderRankingResponse struct {
	EventID                             *int                                 `json:"eventId,omitempty"`
	IsEventAggregate                    *bool                                `json:"isEventAggregate,omitempty"`
	BorderRankings                      []PlayerRankingSchema                `json:"borderRankings,omitempty"`
	UserWorldBloomChapterRankingBorders []UserWorldBloomChapterRankingBorder `json:"userWorldBloomChapterRankingBorders,omitempty"`
}
