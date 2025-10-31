package api

import (
	"context"
	"fmt"
	"strconv"
	"time"

	"haruki-tracker/utils/gorm"
	"haruki-tracker/utils/model"

	"github.com/gofiber/fiber/v2"
)

// API represents the API server
type API struct {
	engines map[model.SekaiServerRegion]*gorm.DatabaseEngine
}

// NewAPI creates a new API instance with database engines for each server
func NewAPI(engines map[model.SekaiServerRegion]*gorm.DatabaseEngine) *API {
	return &API{
		engines: engines,
	}
}

// getEngine retrieves the database engine for a server
func (a *API) getEngine(server string) (*gorm.DatabaseEngine, error) {
	serverRegion := model.SekaiServerRegion(server)
	engine, exists := a.engines[serverRegion]
	if !exists {
		return nil, fmt.Errorf("invalid server: %s", server)
	}
	return engine, nil
}

// RegisterRoutes registers all API routes to the Fiber app
func (a *API) RegisterRoutes(app *fiber.App) {
	// Create API group with prefix
	eventAPI := app.Group("/event/:server/:event_id")

	// Normal event latest ranking endpoints
	eventAPI.Get("/latest-ranking/user/:user_id", a.GetNormalRankingByUserID)
	eventAPI.Get("/latest-ranking/rank/:rank", a.GetNormalRankingByRank)

	// World Bloom latest ranking endpoints
	eventAPI.Get("/latest-world-bloom-ranking/character/:character_id/user/:user_id", a.GetWorldBloomRankingByUserID)
	eventAPI.Get("/latest-world-bloom-ranking/character/:character_id/rank/:rank", a.GetWorldBloomRankingByRank)

	// Normal event trace ranking endpoints
	eventAPI.Get("/trace-ranking/user/:user_id", a.GetAllNormalRankingByUserID)
	eventAPI.Get("/trace-ranking/rank/:rank", a.GetAllNormalRankingByRank)

	// World Bloom trace ranking endpoints
	eventAPI.Get("/trace-world-bloom-ranking/character/:character_id/user/:user_id", a.GetAllWorldBloomRankingByUserID)
	eventAPI.Get("/trace-world-bloom-ranking/character/:character_id/rank/:rank", a.GetAllWorldBloomRankingByRank)

	// User data endpoint
	eventAPI.Get("/user-data/:user_id", a.GetUserDataByUserID)

	// Ranking lines endpoints
	eventAPI.Get("/ranking-lines", a.GetRankingLines)
	eventAPI.Get("/ranking-score-growth/interval/:interval", a.GetRankingScoreGrowths)

	// World Bloom ranking lines endpoints
	eventAPI.Get("/world-bloom-ranking-lines/character/:character_id", a.GetWorldBloomRankingLines)
	eventAPI.Get("/world-bloom-ranking-score-growth/character/:character_id/interval/:interval", a.GetWorldBloomRankingScoreGrowths)
}

// GetNormalRankingByUserID 获取指定活动指定玩家最新排名数据
func (a *API) GetNormalRankingByUserID(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	userID := c.Params("user_id")

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()

	// 获取最新排名
	ranking, err := gorm.FetchLatestRanking(ctx, engine, eventID, userID)
	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	// 获取用户信息
	userData, err := gorm.GetUserNameData(ctx, engine, eventID, userID)
	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	response := model.UserLatestRankingQueryResponseSchema{
		RankData: ranking,
		UserData: userData,
	}

	if ranking == nil && userData == nil {
		return c.Status(404).JSON(fiber.Map{"error": "not found"})
	}

	return c.JSON(response)
}

// GetNormalRankingByRank 获取指定活动指定排名最新排名数据
func (a *API) GetNormalRankingByRank(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	rank, _ := strconv.Atoi(c.Params("rank"))

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()
	table := gorm.GetEventTableModel(eventID)

	var result gorm.EventTable
	err = engine.WithContext(ctx).
		Table(table.TableName()).
		Where("rank = ?", rank).
		Order("timestamp DESC").
		Limit(1).
		First(&result).Error

	if err != nil {
		return c.Status(404).JSON(fiber.Map{"error": "not found"})
	}

	ranking := &model.RecordedRankingSchema{
		Timestamp: result.Timestamp,
		UserID:    result.UserID,
		Score:     result.Score,
		Rank:      result.Rank,
	}

	// 获取用户信息
	userData, _ := gorm.GetUserNameData(ctx, engine, eventID, result.UserID)

	response := model.UserLatestRankingQueryResponseSchema{
		RankData: ranking,
		UserData: userData,
	}

	return c.JSON(response)
}

// GetWorldBloomRankingByUserID 获取指定玩家指定World Link活动指定角色单榜最新排名数据
func (a *API) GetWorldBloomRankingByUserID(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	characterID, _ := strconv.Atoi(c.Params("character_id"))
	userID := c.Params("user_id")

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()

	ranking, err := gorm.FetchLatestWorldBloomRanking(ctx, engine, eventID, userID, characterID)
	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	userData, err := gorm.GetUserNameData(ctx, engine, eventID, userID)
	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	response := model.UserLatestRankingQueryResponseSchema{
		RankData: ranking,
		UserData: userData,
	}

	if ranking == nil && userData == nil {
		return c.Status(404).JSON(fiber.Map{"error": "not found"})
	}

	return c.JSON(response)
}

// GetWorldBloomRankingByRank 获取指定排名指定World Link活动指定角色单榜最新排名数据
func (a *API) GetWorldBloomRankingByRank(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	characterID, _ := strconv.Atoi(c.Params("character_id"))
	rank, _ := strconv.Atoi(c.Params("rank"))

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()
	table := gorm.GetWorldBloomTableModel(eventID)

	var result gorm.WorldBloomTable
	err = engine.WithContext(ctx).
		Table(table.TableName()).
		Where("rank = ? AND character_id = ?", rank, characterID).
		Order("timestamp DESC").
		Limit(1).
		First(&result).Error

	if err != nil {
		return c.Status(404).JSON(fiber.Map{"error": "not found"})
	}

	charID := result.CharacterID
	ranking := &model.RecordedWorldBloomRankingSchema{
		RecordedRankingSchema: model.RecordedRankingSchema{
			Timestamp: result.Timestamp,
			UserID:    result.UserID,
			Score:     result.Score,
			Rank:      result.Rank,
		},
		CharacterID: &charID,
	}

	response := model.UserLatestRankingQueryResponseSchema{
		RankData: ranking,
		UserData: nil,
	}

	return c.JSON(response)
}

// GetAllNormalRankingByUserID 获取指定活动指定玩家的所有已记录的排名数据
func (a *API) GetAllNormalRankingByUserID(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	userID := c.Params("user_id")

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()

	rankings, err := gorm.FetchAllRankings(ctx, engine, eventID, userID)
	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	userData, err := gorm.GetUserNameData(ctx, engine, eventID, userID)
	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	// Convert to interface{} slice
	rankData := make([]interface{}, len(rankings))
	for i, r := range rankings {
		rankData[i] = r
	}

	response := model.UserAllRankingDataQueryResponseSchema{
		RankData: rankData,
		UserData: userData,
	}

	if len(rankings) == 0 && userData == nil {
		return c.Status(404).JSON(fiber.Map{"error": "not found"})
	}

	return c.JSON(response)
}

// GetAllNormalRankingByRank 获取指定活动指定排名的所有已记录的排名数据
func (a *API) GetAllNormalRankingByRank(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	rank, _ := strconv.Atoi(c.Params("rank"))

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()
	table := gorm.GetEventTableModel(eventID)

	var results []gorm.EventTable
	err = engine.WithContext(ctx).
		Table(table.TableName()).
		Where("rank = ?", rank).
		Order("timestamp ASC").
		Find(&results).Error

	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	if len(results) == 0 {
		return c.Status(404).JSON(fiber.Map{"error": "not found"})
	}

	// Convert to RecordedRankingSchema slice
	rankData := make([]interface{}, len(results))
	for i, r := range results {
		rankData[i] = &model.RecordedRankingSchema{
			Timestamp: r.Timestamp,
			UserID:    r.UserID,
			Score:     r.Score,
			Rank:      r.Rank,
		}
	}

	response := model.UserAllRankingDataQueryResponseSchema{
		RankData: rankData,
		UserData: nil,
	}

	return c.JSON(response)
}

// GetAllWorldBloomRankingByUserID 获取指定玩家指定World Link活动指定角色单榜的所有已记录的排名数据
func (a *API) GetAllWorldBloomRankingByUserID(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	characterID, _ := strconv.Atoi(c.Params("character_id"))
	userID := c.Params("user_id")

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()

	rankings, err := gorm.FetchAllWorldBloomRankings(ctx, engine, eventID, userID, characterID)
	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	userData, err := gorm.GetUserNameData(ctx, engine, eventID, userID)
	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	// Convert to interface{} slice
	rankData := make([]interface{}, len(rankings))
	for i, r := range rankings {
		rankData[i] = r
	}

	response := model.UserAllRankingDataQueryResponseSchema{
		RankData: rankData,
		UserData: userData,
	}

	if len(rankings) == 0 && userData == nil {
		return c.Status(404).JSON(fiber.Map{"error": "not found"})
	}

	return c.JSON(response)
}

// GetAllWorldBloomRankingByRank 获取指定排名指定World Link活动指定角色单榜的所有已记录的排名数据
func (a *API) GetAllWorldBloomRankingByRank(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	characterID, _ := strconv.Atoi(c.Params("character_id"))
	rank, _ := strconv.Atoi(c.Params("rank"))

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()
	table := gorm.GetWorldBloomTableModel(eventID)

	var results []gorm.WorldBloomTable
	err = engine.WithContext(ctx).
		Table(table.TableName()).
		Where("rank = ? AND character_id = ?", rank, characterID).
		Order("timestamp ASC").
		Find(&results).Error

	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	if len(results) == 0 {
		return c.Status(404).JSON(fiber.Map{"error": "not found"})
	}

	// Convert to RecordedWorldBloomRankingSchema slice
	rankData := make([]interface{}, len(results))
	for i, r := range results {
		charID := r.CharacterID
		rankData[i] = &model.RecordedWorldBloomRankingSchema{
			RecordedRankingSchema: model.RecordedRankingSchema{
				Timestamp: r.Timestamp,
				UserID:    r.UserID,
				Score:     r.Score,
				Rank:      r.Rank,
			},
			CharacterID: &charID,
		}
	}

	response := model.UserAllRankingDataQueryResponseSchema{
		RankData: rankData,
		UserData: nil,
	}

	return c.JSON(response)
}

// GetUserDataByUserID 获取指定用户的基础信息
func (a *API) GetUserDataByUserID(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	userID := c.Params("user_id")

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()

	userData, err := gorm.GetUserNameData(ctx, engine, eventID, userID)
	if err != nil {
		return c.Status(500).JSON(fiber.Map{"error": err.Error()})
	}

	if userData == nil {
		return c.Status(404).JSON(fiber.Map{"error": "not found"})
	}

	return c.JSON(userData)
}

// GetRankingLines 获取指定活动最新分数线
func (a *API) GetRankingLines(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()
	table := gorm.GetEventTableModel(eventID)

	result := make([]*model.RankingLineScoreSchema, 0)

	// 使用 NORMAL 排名线
	for _, rank := range model.SekaiEventRankingLinesNormal {
		var record gorm.EventTable
		err := engine.WithContext(ctx).
			Table(table.TableName()).
			Where("rank = ?", rank).
			Order("timestamp DESC").
			Limit(1).
			First(&record).Error

		if err == nil {
			result = append(result, &model.RankingLineScoreSchema{
				Rank:      record.Rank,
				Score:     record.Score,
				Timestamp: record.Timestamp,
			})
		}
	}

	return c.JSON(result)
}

// GetRankingScoreGrowths 获取指定活动排名的分数增长速度
func (a *API) GetRankingScoreGrowths(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	interval, _ := strconv.Atoi(c.Params("interval"))

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()
	table := gorm.GetEventTableModel(eventID)

	result := make([]*model.RankingScoreGrowthSchema, 0)
	now := time.Now().Unix()
	startTime := now - int64(interval)

	for _, rank := range model.SekaiEventRankingLinesNormal {
		var records []gorm.EventTable
		err := engine.WithContext(ctx).
			Table(table.TableName()).
			Where("rank = ? AND timestamp >= ?", rank, startTime).
			Order("timestamp ASC").
			Find(&records).Error

		if err == nil && len(records) >= 2 {
			earlier := records[0]
			latest := records[len(records)-1]
			growth := latest.Score - earlier.Score

			earlierTS := earlier.Timestamp
			earlierScore := earlier.Score

			result = append(result, &model.RankingScoreGrowthSchema{
				Rank:             rank,
				TimestampLatest:  latest.Timestamp,
				ScoreLatest:      latest.Score,
				TimestampEarlier: &earlierTS,
				ScoreEarlier:     &earlierScore,
				Growth:           &growth,
			})
		}
	}

	return c.JSON(result)
}

// GetWorldBloomRankingLines 获取指定World Link活动指定角色单榜排名最新分数线
func (a *API) GetWorldBloomRankingLines(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	characterID, _ := strconv.Atoi(c.Params("character_id"))

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()
	table := gorm.GetWorldBloomTableModel(eventID)

	result := make([]*model.RankingLineScoreSchema, 0)

	for _, rank := range model.SekaiEventRankingLinesWorldBloom {
		var record gorm.WorldBloomTable
		err := engine.WithContext(ctx).
			Table(table.TableName()).
			Where("rank = ? AND character_id = ?", rank, characterID).
			Order("timestamp DESC").
			Limit(1).
			First(&record).Error

		if err == nil {
			result = append(result, &model.RankingLineScoreSchema{
				Rank:      record.Rank,
				Score:     record.Score,
				Timestamp: record.Timestamp,
			})
		}
	}

	return c.JSON(result)
}

// GetWorldBloomRankingScoreGrowths 获取指定World Link活动指定角色单榜排名的分数增长速度
func (a *API) GetWorldBloomRankingScoreGrowths(c *fiber.Ctx) error {
	server := c.Params("server")
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	characterID, _ := strconv.Atoi(c.Params("character_id"))
	interval, _ := strconv.Atoi(c.Params("interval"))

	engine, err := a.getEngine(server)
	if err != nil {
		return c.Status(400).JSON(fiber.Map{"error": err.Error()})
	}

	ctx := context.Background()
	table := gorm.GetWorldBloomTableModel(eventID)

	result := make([]*model.RankingScoreGrowthSchema, 0)
	now := time.Now().Unix()
	startTime := now - int64(interval)

	for _, rank := range model.SekaiEventRankingLinesWorldBloom {
		var records []gorm.WorldBloomTable
		err := engine.WithContext(ctx).
			Table(table.TableName()).
			Where("rank = ? AND character_id = ? AND timestamp >= ?", rank, characterID, startTime).
			Order("timestamp ASC").
			Find(&records).Error

		if err == nil && len(records) >= 2 {
			earlier := records[0]
			latest := records[len(records)-1]
			growth := latest.Score - earlier.Score

			earlierTS := earlier.Timestamp
			earlierScore := earlier.Score

			result = append(result, &model.RankingScoreGrowthSchema{
				Rank:             rank,
				TimestampLatest:  latest.Timestamp,
				ScoreLatest:      latest.Score,
				TimestampEarlier: &earlierTS,
				ScoreEarlier:     &earlierScore,
				Growth:           &growth,
			})
		}
	}

	return c.JSON(result)
}
