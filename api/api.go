package api

import (
	"fmt"

	"haruki-tracker/utils/gorm"
	"haruki-tracker/utils/model"

	"github.com/gofiber/fiber/v2"
)

// getEngine retrieves the database engine for a server from the global sekaiDBs
func getEngine(server string) (*gorm.DatabaseEngine, error) {
	serverRegion := model.SekaiServerRegion(server)
	engine, exists := sekaiDBs[serverRegion]
	if !exists {
		return nil, fmt.Errorf("invalid server: %s", server)
	}
	return engine, nil
}

// RegisterRoutes registers all API routes to the Fiber app
func RegisterRoutes(app *fiber.App) {
	eventAPI := app.Group("/event/:server/:event_id")
	eventAPI.Get("/latest-ranking/user/:user_id", GetNormalRankingByUserID)
	eventAPI.Get("/latest-ranking/rank/:rank", GetNormalRankingByRank)
	eventAPI.Get("/latest-world-bloom-ranking/character/:character_id/user/:user_id", GetWorldBloomRankingByUserID)
	eventAPI.Get("/latest-world-bloom-ranking/character/:character_id/rank/:rank", GetWorldBloomRankingByRank)
	eventAPI.Get("/trace-ranking/user/:user_id", GetAllNormalRankingByUserID)
	eventAPI.Get("/trace-ranking/rank/:rank", GetAllNormalRankingByRank)
	eventAPI.Get("/trace-world-bloom-ranking/character/:character_id/user/:user_id", GetAllWorldBloomRankingByUserID)
	eventAPI.Get("/trace-world-bloom-ranking/character/:character_id/rank/:rank", GetAllWorldBloomRankingByRank)
	eventAPI.Get("/user-data/:user_id", GetUserDataByUserID)
	eventAPI.Get("/ranking-lines", GetRankingLines)
	eventAPI.Get("/ranking-score-growth/interval/:interval", GetRankingScoreGrowths)
	eventAPI.Get("/world-bloom-ranking-lines/character/:character_id", GetWorldBloomRankingLines)
	eventAPI.Get("/world-bloom-ranking-score-growth/character/:character_id/interval/:interval", GetWorldBloomRankingScoreGrowths)
}

// GetNormalRankingByUserID 获取指定活动指定玩家最新排名数据
func GetNormalRankingByUserID(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetNormalRankingByRank 获取指定活动指定排名最新排名数据
func GetNormalRankingByRank(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetWorldBloomRankingByUserID 获取指定玩家指定World Link活动指定角色单榜最新排名数据
func GetWorldBloomRankingByUserID(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetWorldBloomRankingByRank 获取指定排名指定World Link活动指定角色单榜最新排名数据
func GetWorldBloomRankingByRank(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetAllNormalRankingByUserID 获取指定活动指定玩家的所有已记录的排名数据
func GetAllNormalRankingByUserID(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetAllNormalRankingByRank 获取指定活动指定排名的所有已记录的排名数据
func GetAllNormalRankingByRank(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetAllWorldBloomRankingByUserID 获取指定玩家指定World Link活动指定角色单榜的所有已记录的排名数据
func GetAllWorldBloomRankingByUserID(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetAllWorldBloomRankingByRank 获取指定排名指定World Link活动指定角色单榜的所有已记录的排名数据
func GetAllWorldBloomRankingByRank(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetUserDataByUserID 获取指定用户的基础信息
func GetUserDataByUserID(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetRankingLines 获取指定活动最新分数线
func GetRankingLines(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetRankingScoreGrowths 获取指定活动排名的分数增长速度
func GetRankingScoreGrowths(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetWorldBloomRankingLines 获取指定World Link活动指定角色单榜排名最新分数线
func GetWorldBloomRankingLines(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}

// GetWorldBloomRankingScoreGrowths 获取指定World Link活动指定角色单榜排名的分数增长速度
func GetWorldBloomRankingScoreGrowths(c *fiber.Ctx) error {
	// TODO: 待实现
	return c.Status(501).JSON(fiber.Map{"error": "not implemented"})
}
