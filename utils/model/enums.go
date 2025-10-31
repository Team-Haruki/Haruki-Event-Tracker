package model

// SekaiServerRegion represents the server region
type SekaiServerRegion string

const (
	SekaiServerRegionJP SekaiServerRegion = "jp"
	SekaiServerRegionEN SekaiServerRegion = "en"
	SekaiServerRegionTW SekaiServerRegion = "tw"
	SekaiServerRegionKR SekaiServerRegion = "kr"
	SekaiServerRegionCN SekaiServerRegion = "cn"
)

// SekaiEventType represents the event type
type SekaiEventType string

const (
	SekaiEventTypeMarathon         SekaiEventType = "marathon"          // 马拉松 (普活)
	SekaiEventTypeCheerfulCarnival SekaiEventType = "cheerful_carnival" // 欢乐嘉年华 (5v5)
	SekaiEventTypeWorldBloom       SekaiEventType = "world_bloom"       // 世界连接 (World Link)
)

// SekaiEventStatus represents the event status
type SekaiEventStatus string

const (
	SekaiEventStatusNotStarted  SekaiEventStatus = "not_started" // 还没开始
	SekaiEventStatusOngoing     SekaiEventStatus = "ongoing"     // 正在进行
	SekaiEventStatusAggregating SekaiEventStatus = "aggregating" // 集算中
	SekaiEventStatusEnded       SekaiEventStatus = "ended"       // 已结束
)

// SekaiEventSpeedType represents the event speed type
type SekaiEventSpeedType string

const (
	SekaiEventSpeedTypeHourly    SekaiEventSpeedType = "hourly"
	SekaiEventSpeedTypeSemiDaily SekaiEventSpeedType = "semi_daily"
	SekaiEventSpeedTypeDaily     SekaiEventSpeedType = "daily"
)

// SekaiEventRankingLinesNormal represents the normal event ranking lines
var SekaiEventRankingLinesNormal = []int{
	10, 20, 30, 40, 50, 100, 200, 300, 400, 500,
	1000, 1500, 2000, 2500, 3000, 4000, 5000,
	10000, 20000, 30000, 40000, 50000,
	100000, 200000, 300000,
}

// SekaiEventRankingLinesWorldBloom represents the world bloom event ranking lines
var SekaiEventRankingLinesWorldBloom = []int{
	10, 20, 30, 40, 50, 100, 200, 300, 400, 500,
	1000, 2000, 3000, 4000, 5000, 7000,
	10000, 20000, 30000, 40000, 50000, 70000,
	100000,
}
