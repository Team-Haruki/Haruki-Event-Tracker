package model

type UserCard struct {
	CardID                *int    `json:"cardId,omitempty"`
	DefaultImage          *string `json:"defaultImage,omitempty"`
	Level                 *int    `json:"level,omitempty"`
	MasterRank            *int    `json:"masterRank,omitempty"`
	SpecialTrainingStatus *string `json:"specialTrainingStatus,omitempty"`
}

type UserCheerfulCarnival struct {
	CheerfulCarnivalTeamID *int   `json:"cheerfulCarnivalTeamId,omitempty"`
	EventID                *int   `json:"eventId,omitempty"`
	RegisterAt             *int64 `json:"registerAt,omitempty"`
	TeamChangeCount        *int   `json:"teamChangeCount,omitempty"`
}

type UserProfile struct {
	ProfileImageType *string `json:"profileImageType,omitempty"`
	TwitterID        *string `json:"twitterId,omitempty"`
	UserID           *int    `json:"userId,omitempty"`
	Word             *string `json:"word,omitempty"`
}

type UserProfileHonor struct {
	BondsHonorViewType *string `json:"bondsHonorViewType,omitempty"`
	BondsHonorWordID   *int    `json:"bondsHonorWordId,omitempty"`
	HonorID            *int    `json:"honorId,omitempty"`
	HonorLevel         *int    `json:"honorLevel,omitempty"`
	ProfileHonorType   *string `json:"profileHonorType,omitempty"`
	Seq                *int    `json:"seq,omitempty"`
}

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

type UserWorldBloomChapterRankingBase struct {
	EventID                      *int  `json:"eventId,omitempty"`
	GameCharacterID              *int  `json:"gameCharacterId,omitempty"`
	IsWorldBloomChapterAggregate *bool `json:"isWorldBloomChapterAggregate,omitempty"`
}

type UserWorldBloomChapterRanking struct {
	UserWorldBloomChapterRankingBase
	Rankings []PlayerRankingSchema `json:"rankings,omitempty"`
}

type UserWorldBloomChapterRankingBorder struct {
	UserWorldBloomChapterRankingBase
	BorderRankings []PlayerRankingSchema `json:"borderRankings,omitempty"`
}

type Top100RankingResponse struct {
	IsEventAggregate              *bool                          `json:"isEventAggregate,omitempty"`
	Rankings                      []PlayerRankingSchema          `json:"rankings,omitempty"`
	UserRankingStatus             string                         `json:"userRankingStatus"`
	UserWorldBloomChapterRankings []UserWorldBloomChapterRanking `json:"userWorldBloomChapterRankings,omitempty"`
}

type BorderRankingResponse struct {
	EventID                             *int                                 `json:"eventId,omitempty"`
	IsEventAggregate                    *bool                                `json:"isEventAggregate,omitempty"`
	BorderRankings                      []PlayerRankingSchema                `json:"borderRankings,omitempty"`
	UserWorldBloomChapterRankingBorders []UserWorldBloomChapterRankingBorder `json:"userWorldBloomChapterRankingBorders,omitempty"`
}
