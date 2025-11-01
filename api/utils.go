package api

import (
	"context"
	"fmt"

	"haruki-tracker/config"
	"haruki-tracker/tracker"
	"haruki-tracker/utils/gorm"
	"haruki-tracker/utils/logger"
	"haruki-tracker/utils/model"

	"github.com/go-co-op/gocron/v2"
	"github.com/redis/go-redis/v9"
)

var (
	sekaiAPIClient      *tracker.HarukiSekaiAPIClient
	sekaiRedis          *redis.Client
	sekaiDBs            map[model.SekaiServerRegion]*gorm.DatabaseEngine
	sekaiTrackerDaemons map[model.SekaiServerRegion]*tracker.HarukiEventTracker
	sekaiAPIUtilsLogger *logger.Logger
	sekaiScheduler      gocron.Scheduler
)

func InitAPIUtils(cfg config.Config) error {
	var err error
	sekaiAPIUtilsLogger = logger.NewLogger("HarukiTrackerAPIUtils", cfg.Backend.LogLevel, nil)
	sekaiAPIUtilsLogger.Infof("Initializing Haruki Event Tracker API...")
	if cfg.Redis.Enabled {
		sekaiAPIUtilsLogger.Infof("Initializing Redis client...")
		sekaiRedis = redis.NewClient(&redis.Options{
			Addr:     fmt.Sprintf("%s:%d", cfg.Redis.Host, cfg.Redis.Port),
			Password: cfg.Redis.Password,
			DB:       0,
		})
		ctx := context.Background()
		if err := sekaiRedis.Ping(ctx).Err(); err != nil {
			return fmt.Errorf("failed to connect to Redis: %w", err)
		}
		sekaiAPIUtilsLogger.Infof("Redis client initialized successfully")
	} else {
		sekaiAPIUtilsLogger.Warnf("Redis is disabled in configuration")
	}
	sekaiAPIUtilsLogger.Infof("Initializing Sekai API client...")
	sekaiAPIClient = tracker.NewHarukiSekaiAPIClient(cfg.SekaiAPI.APIEndpoint, cfg.SekaiAPI.APIToken)
	sekaiDBs = make(map[model.SekaiServerRegion]*gorm.DatabaseEngine)
	sekaiTrackerDaemons = make(map[model.SekaiServerRegion]*tracker.HarukiEventTracker)
	sekaiAPIUtilsLogger.Infof("Initializing scheduler...")
	sekaiScheduler, err = gocron.NewScheduler()
	if err != nil {
		return fmt.Errorf("failed to create scheduler: %w", err)
	}
	sekaiScheduler.Start()
	sekaiAPIUtilsLogger.Infof("Scheduler started successfully")
	for server, serverCfg := range cfg.Servers {
		if !serverCfg.Enabled {
			sekaiAPIUtilsLogger.Infof("Server %s is disabled, skipping...", server)
			continue
		}
		sekaiAPIUtilsLogger.Infof("Initializing server: %s", server)
		if serverCfg.GormConfig.Enabled {
			sekaiAPIUtilsLogger.Infof("Creating database engine for %s...", server)
			engine, err := gorm.NewDatabaseEngine(serverCfg.GormConfig)
			if err != nil {
				return fmt.Errorf("failed to create database engine for %s: %w", server, err)
			}
			sekaiDBs[server] = engine
			sekaiAPIUtilsLogger.Infof("Database engine for %s initialized successfully", server)
		} else {
			sekaiAPIUtilsLogger.Warnf("Database is disabled for server %s", server)
			continue
		}
		sekaiAPIUtilsLogger.Infof("Creating event tracker daemon for %s...", server)
		trackerDaemon := tracker.NewHarukiEventTracker(
			server,
			sekaiAPIClient,
			sekaiRedis,
			sekaiDBs[server],
			serverCfg.MasterDataDir,
		)
		if err := trackerDaemon.Init(); err != nil {
			sekaiAPIUtilsLogger.Warnf("Failed to initialize tracker daemon for %s: %v, will retry on first run", server, err)
		}
		sekaiTrackerDaemons[server] = trackerDaemon
		sekaiAPIUtilsLogger.Infof("Event tracker daemon for %s initialized successfully", server)
		cronExpr := serverCfg.TrackerCron
		sekaiAPIUtilsLogger.Infof("Registering tracker cron job for %s with expression: %s", server, cronExpr)
		_, err = sekaiScheduler.NewJob(
			gocron.CronJob(cronExpr, false),
			gocron.NewTask(func(s model.SekaiServerRegion) {
				daemon := sekaiTrackerDaemons[s]
				if daemon == nil {
					sekaiAPIUtilsLogger.Errorf("Tracker daemon for %s not found", s)
					return
				}
				sekaiAPIUtilsLogger.Infof("Running tracker for %s...", s)
				daemon.TrackRankingData()
				sekaiAPIUtilsLogger.Infof("Successfully tracked ranking data for %s", s)
			}, server),
			gocron.WithName(fmt.Sprintf("tracker-%s", server)),
		)
		if err != nil {
			return fmt.Errorf("failed to register cron job for %s: %w", server, err)
		}
		sekaiAPIUtilsLogger.Infof("Cron job registered for %s", server)
	}
	sekaiAPIUtilsLogger.Infof("Haruki Event Tracker API initialized successfully")
	return nil
}

func Shutdown() error {
	sekaiAPIUtilsLogger.Infof("Shutting down Haruki Event Tracker API...")
	if sekaiScheduler != nil {
		if err := sekaiScheduler.Shutdown(); err != nil {
			sekaiAPIUtilsLogger.Errorf("Failed to shutdown scheduler: %v", err)
		}
	}
	if sekaiRedis != nil {
		if err := sekaiRedis.Close(); err != nil {
			sekaiAPIUtilsLogger.Errorf("Failed to close Redis: %v", err)
		}
	}
	for server, engine := range sekaiDBs {
		if err := engine.Close(); err != nil {
			sekaiAPIUtilsLogger.Errorf("Failed to close database for %s: %v", server, err)
		}
	}
	sekaiAPIUtilsLogger.Infof("Shutdown completed")
	return nil
}
