package api

import (
	"context"
	"fmt"
	"strconv"

	"haruki-tracker/utils/gorm"
	"haruki-tracker/utils/model"

	"github.com/gofiber/fiber/v2"
)

type commonParams struct {
	ctx          context.Context
	engine       *gorm.DatabaseEngine
	serverRegion model.SekaiServerRegion
	eventID      int
	userID       string
	rank         int
	characterID  int
	interval     int64
}

func parseCommonParams(c *fiber.Ctx) (*commonParams, error) {
	server := c.Params("server")
	serverRegion := model.SekaiServerRegion(server)
	engine, exists := sekaiDBs[serverRegion]
	if !exists {
		return nil, fmt.Errorf("invalid server: %s", server)
	}
	eventID, _ := strconv.Atoi(c.Params("event_id"))
	params := &commonParams{
		ctx:          context.Background(),
		engine:       engine,
		serverRegion: serverRegion,
		eventID:      eventID,
		userID:       c.Params("user_id"),
	}
	if rank := c.Params("rank"); rank != "" {
		params.rank, _ = strconv.Atoi(rank)
	}
	if characterID := c.Params("character_id"); characterID != "" {
		params.characterID, _ = strconv.Atoi(characterID)
	}
	if interval := c.Params("interval"); interval != "" {
		params.interval, _ = strconv.ParseInt(interval, 10, 64)
	}
	return params, nil
}

func getNormalRankingByUserID(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	ranking, err := gorm.FetchLatestRanking(p.ctx, p.engine, p.serverRegion, p.eventID, p.userID)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	userData, _ := gorm.GetUserData(p.ctx, p.engine, p.serverRegion, p.eventID, p.userID)
	if ranking == nil && userData == nil {
		return c.Status(fiber.StatusNotFound).JSON(fiber.Map{"error": "not found"})
	}
	return c.JSON(model.UserLatestRankingQueryResponseSchema{
		RankData: ranking,
		UserData: userData,
	})
}

func getNormalRankingByRank(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	ranking, err := gorm.FetchLatestRankingByRank(p.ctx, p.engine, p.serverRegion, p.eventID, p.rank)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	if ranking == nil {
		return c.Status(fiber.StatusNotFound).JSON(fiber.Map{"error": "not found"})
	}
	userData, _ := gorm.GetUserData(p.ctx, p.engine, p.serverRegion, p.eventID, ranking.UserID)
	return c.JSON(model.UserLatestRankingQueryResponseSchema{
		RankData: ranking,
		UserData: userData,
	})
}

func getWorldBloomRankingByUserID(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	ranking, err := gorm.FetchLatestWorldBloomRanking(p.ctx, p.engine, p.serverRegion, p.eventID, p.userID, p.characterID)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	userData, _ := gorm.GetUserData(p.ctx, p.engine, p.serverRegion, p.eventID, p.userID)
	if ranking == nil && userData == nil {
		return c.Status(fiber.StatusNotFound).JSON(fiber.Map{"error": "not found"})
	}
	return c.JSON(model.UserLatestRankingQueryResponseSchema{
		RankData: ranking,
		UserData: userData,
	})
}

func getWorldBloomRankingByRank(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	ranking, err := gorm.FetchLatestWorldBloomRankingByRank(p.ctx, p.engine, p.serverRegion, p.eventID, p.rank, p.characterID)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	if ranking == nil {
		return c.Status(fiber.StatusNotFound).JSON(fiber.Map{"error": "not found"})
	}
	userData, _ := gorm.GetUserData(p.ctx, p.engine, p.serverRegion, p.eventID, ranking.UserID)
	return c.JSON(model.UserLatestRankingQueryResponseSchema{
		RankData: ranking,
		UserData: userData,
	})
}

func getAllNormalRankingByUserID(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	rankings, err := gorm.FetchAllRankings(p.ctx, p.engine, p.serverRegion, p.eventID, p.userID)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	userData, _ := gorm.GetUserData(p.ctx, p.engine, p.serverRegion, p.eventID, p.userID)
	if len(rankings) == 0 && userData == nil {
		return c.Status(fiber.StatusNotFound).JSON(fiber.Map{"error": "not found"})
	}
	rankData := make([]interface{}, len(rankings))
	for i, r := range rankings {
		rankData[i] = r
	}
	return c.JSON(model.UserAllRankingDataQueryResponseSchema{
		RankData: rankData,
		UserData: userData,
	})
}

func getAllNormalRankingByRank(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	rankings, err := gorm.FetchAllRankingsByRank(p.ctx, p.engine, p.serverRegion, p.eventID, p.rank)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	if len(rankings) == 0 {
		return c.Status(fiber.StatusNotFound).JSON(fiber.Map{"error": "not found"})
	}
	rankData := make([]interface{}, len(rankings))
	for i, r := range rankings {
		rankData[i] = r
	}
	return c.JSON(model.UserAllRankingDataQueryResponseSchema{
		RankData: rankData,
		UserData: nil,
	})
}

func getAllWorldBloomRankingByUserID(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	rankings, err := gorm.FetchAllWorldBloomRankings(p.ctx, p.engine, p.serverRegion, p.eventID, p.userID, p.characterID)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	userData, _ := gorm.GetUserData(p.ctx, p.engine, p.serverRegion, p.eventID, p.userID)
	if len(rankings) == 0 && userData == nil {
		return c.Status(fiber.StatusNotFound).JSON(fiber.Map{"error": "not found"})
	}
	rankData := make([]interface{}, len(rankings))
	for i, r := range rankings {
		rankData[i] = r
	}
	return c.JSON(model.UserAllRankingDataQueryResponseSchema{
		RankData: rankData,
		UserData: userData,
	})
}

func getAllWorldBloomRankingByRank(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	rankings, err := gorm.FetchAllWorldBloomRankingsByRank(p.ctx, p.engine, p.serverRegion, p.eventID, p.rank, p.characterID)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	if len(rankings) == 0 {
		return c.Status(fiber.StatusNotFound).JSON(fiber.Map{"error": "not found"})
	}
	rankData := make([]interface{}, len(rankings))
	for i, r := range rankings {
		rankData[i] = r
	}
	return c.JSON(model.UserAllRankingDataQueryResponseSchema{
		RankData: rankData,
		UserData: nil,
	})
}

func getUserDataByUserID(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	userData, err := gorm.GetUserData(p.ctx, p.engine, p.serverRegion, p.eventID, p.userID)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	if userData == nil {
		return c.Status(fiber.StatusNotFound).JSON(fiber.Map{"error": "not found"})
	}
	return c.JSON(userData)
}

func getRankingLines(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}

	result, err := gorm.FetchRankingLines(p.ctx, p.engine, p.serverRegion, p.eventID, model.SekaiEventRankingLinesNormal)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}

	return c.JSON(result)
}

func getRankingScoreGrowths(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	currentTime := c.Context().Time().Unix()
	startTime := currentTime - p.interval
	result, err := gorm.FetchRankingScoreGrowths(p.ctx, p.engine, p.serverRegion, p.eventID, model.SekaiEventRankingLinesNormal, startTime)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	return c.JSON(result)
}

func getWorldBloomRankingLines(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}

	result, err := gorm.FetchWorldBloomRankingLines(p.ctx, p.engine, p.serverRegion, p.eventID, p.characterID, model.SekaiEventRankingLinesWorldBloom)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}

	return c.JSON(result)
}

func getWorldBloomRankingScoreGrowths(c *fiber.Ctx) error {
	p, err := parseCommonParams(c)
	if err != nil {
		return c.Status(fiber.StatusBadRequest).JSON(fiber.Map{"error": err.Error()})
	}
	currentTime := c.Context().Time().Unix()
	startTime := currentTime - p.interval
	result, err := gorm.FetchWorldBloomRankingScoreGrowths(p.ctx, p.engine, p.serverRegion, p.eventID, p.characterID, model.SekaiEventRankingLinesWorldBloom, startTime)
	if err != nil {
		return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{"error": err.Error()})
	}
	return c.JSON(result)
}

func RegisterRoutes(app *fiber.App) {
	eventAPI := app.Group("/event/:server/:event_id")
	eventAPI.Get("/latest-ranking/user/:user_id", getNormalRankingByUserID)
	eventAPI.Get("/latest-ranking/rank/:rank", getNormalRankingByRank)
	eventAPI.Get("/latest-world-bloom-ranking/character/:character_id/user/:user_id", getWorldBloomRankingByUserID)
	eventAPI.Get("/latest-world-bloom-ranking/character/:character_id/rank/:rank", getWorldBloomRankingByRank)
	eventAPI.Get("/trace-ranking/user/:user_id", getAllNormalRankingByUserID)
	eventAPI.Get("/trace-ranking/rank/:rank", getAllNormalRankingByRank)
	eventAPI.Get("/trace-world-bloom-ranking/character/:character_id/user/:user_id", getAllWorldBloomRankingByUserID)
	eventAPI.Get("/trace-world-bloom-ranking/character/:character_id/rank/:rank", getAllWorldBloomRankingByRank)
	eventAPI.Get("/user-data/:user_id", getUserDataByUserID)
	eventAPI.Get("/ranking-lines", getRankingLines)
	eventAPI.Get("/ranking-score-growth/interval/:interval", getRankingScoreGrowths)
	eventAPI.Get("/world-bloom-ranking-lines/character/:character_id", getWorldBloomRankingLines)
	eventAPI.Get("/world-bloom-ranking-score-growth/character/:character_id/interval/:interval", getWorldBloomRankingScoreGrowths)
}
