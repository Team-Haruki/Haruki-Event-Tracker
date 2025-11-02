package model

type SekaiServerRegion string

const (
	SekaiServerRegionJP SekaiServerRegion = "jp"
	SekaiServerRegionEN SekaiServerRegion = "en"
	SekaiServerRegionTW SekaiServerRegion = "tw"
	SekaiServerRegionKR SekaiServerRegion = "kr"
	SekaiServerRegionCN SekaiServerRegion = "cn"
)

type SekaiEventType string

const (
	SekaiEventTypeMarathon         SekaiEventType = "marathon"          // 马拉松 (普活)
	SekaiEventTypeCheerfulCarnival SekaiEventType = "cheerful_carnival" // 欢乐嘉年华 (5v5)
	SekaiEventTypeWorldBloom       SekaiEventType = "world_bloom"       // 世界连接 (World Link)
)

type SekaiWorldBloomType string

const (
	SekaiWorldBloomTypeGameCharacter SekaiWorldBloomType = "game_character"
	SekaiWorldBloomTypeFinale        SekaiWorldBloomType = "finale"
)

type SekaiEventStatus string

const (
	SekaiEventStatusNotStarted  SekaiEventStatus = "not_started" // 还没开始
	SekaiEventStatusOngoing     SekaiEventStatus = "ongoing"     // 正在进行
	SekaiEventStatusAggregating SekaiEventStatus = "aggregating" // 集算中
	SekaiEventStatusEnded       SekaiEventStatus = "ended"       // 已结束
)

type SekaiEventSpeedType string

const (
	SekaiEventSpeedTypeHourly    SekaiEventSpeedType = "hourly"
	SekaiEventSpeedTypeSemiDaily SekaiEventSpeedType = "semi_daily"
	SekaiEventSpeedTypeDaily     SekaiEventSpeedType = "daily"
)

type SekaiUnit string

const (
	SekaiUnitNone                SekaiUnit = "none"
	SekaiUnitLeoneed             SekaiUnit = "light_sound"
	SekaiUnitMoreMoreJump        SekaiUnit = "idol"
	SekaiUnitVividBadSquad       SekaiUnit = "street"
	SekaiUnitWonderlandsShowtime SekaiUnit = "theme_park"
	SekaiUnitNightcord           SekaiUnit = "school_refusal"
)

var SekaiEventRankingLinesNormal = []int{
	10, 20, 30, 40, 50, 100, 200, 300, 400, 500,
	1000, 1500, 2000, 2500, 3000, 4000, 5000,
	10000, 20000, 30000, 40000, 50000,
	100000, 200000, 300000,
}

var SekaiEventRankingLinesWorldBloom = []int{
	10, 20, 30, 40, 50, 100, 200, 300, 400, 500,
	1000, 2000, 3000, 4000, 5000, 7000,
	10000, 20000, 30000, 40000, 50000, 70000,
	100000,
}
