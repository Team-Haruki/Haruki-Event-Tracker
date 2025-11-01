package model

type WorldBloomChapterStatus struct {
	Server        SekaiServerRegion `json:"server"`
	EventID       int               `json:"event_id"`
	CharacterID   int               `json:"character_id"`
	ChapterStatus SekaiEventStatus  `json:"chapter_status"`
}

type EventStatus struct {
	Server          SekaiServerRegion               `json:"server"`
	EventID         int                             `json:"event_id"`
	EventType       SekaiEventType                  `json:"event_type"`
	EventStatus     SekaiEventStatus                `json:"event_status"`
	Remain          string                          `json:"remain"`
	AssetbundleName string                          `json:"assetbundle_name"`
	ChapterStatuses map[int]WorldBloomChapterStatus `json:"chapter_statuses,omitempty"`
	Detail          Event                           `json:"detail"`
}

type WorldBloom struct {
	ID                    int                 `json:"id"`
	EventID               int                 `json:"eventId"`
	GameCharacterID       int                 `json:"gameCharacterId,omitempty"`
	WorldBloomChapterType SekaiWorldBloomType `json:"worldBloomChapterType"`
	ChapterNo             int                 `json:"chapterNo"`
	ChapterStartAt        int64               `json:"chapterStartAt"`
	AggregateAt           int64               `json:"aggregateAt"`
	ChapterEndAt          int64               `json:"chapterEndAt"`
	IsSupplemental        bool                `json:"isSupplemental"`
}

type Event struct {
	ID                               int                       `json:"id"`
	EventType                        SekaiEventType            `json:"eventType"`
	Name                             string                    `json:"name"`
	AssetbundleName                  string                    `json:"assetbundleName"`
	BgmAssetbundleName               string                    `json:"bgmAssetbundleName"`
	EventOnlyComponentDisplayStartAt int64                     `json:"eventOnlyComponentDisplayStartAt"`
	StartAt                          int64                     `json:"startAt"`
	AggregateAt                      int64                     `json:"aggregateAt"`
	RankingAnnounceAt                int64                     `json:"rankingAnnounceAt"`
	DistributionStartAt              int64                     `json:"distributionStartAt"`
	EventOnlyComponentDisplayEndAt   int64                     `json:"eventOnlyComponentDisplayEndAt"`
	ClosedAt                         int64                     `json:"closedAt"`
	DistributionEndAt                int64                     `json:"distributionEndAt"`
	VirtualLiveID                    int                       `json:"virtualLiveId,omitempty"`
	Unit                             SekaiUnit                 `json:"unit"`
	IsCountLeaderCharacterPlay       bool                      `json:"isCountLeaderCharacterPlay"`
	EventRankingRewardRanges         []EventRankingRewardRange `json:"eventRankingRewardRanges"`
	EventPointAssetbundleName        string                    `json:"eventPointAssetbundleName,omitempty"`
	StandbyScreenDisplayStartAt      int64                     `json:"standbyScreenDisplayStartAt,omitempty"`
}

type EventRankingRewardRange struct {
	ID                  int                  `json:"id"`
	EventID             int                  `json:"eventId"`
	FromRank            int                  `json:"fromRank"`
	ToRank              int                  `json:"toRank"`
	IsToRankBorder      bool                 `json:"isToRankBorder"`
	EventRankingRewards []EventRankingReward `json:"eventRankingRewards"`
}

type EventRankingReward struct {
	ID                        int    `json:"id"`
	EventRankingRewardRangeID int    `json:"eventRankingRewardRangeId"`
	Seq                       int    `json:"seq"`
	ResourceBoxID             int    `json:"resourceBoxId"`
	RewardConditionType       string `json:"rewardConditionType"`
}
