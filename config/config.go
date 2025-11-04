package config

import (
	harukiLogger "haruki-tracker/utils/logger"
	"haruki-tracker/utils/model"
	"os"

	"gopkg.in/yaml.v3"
)

var Version = "2.0.0-dev"
var Cfg Config

type RedisConfig struct {
	Host     string `yaml:"host"`
	Port     int    `yaml:"port"`
	Password string `yaml:"password"`
}

type SekaiAPIConfig struct {
	APIEndpoint string `yaml:"api_endpoint"`
	APIToken    string `yaml:"api_token"`
}

type BackendConfig struct {
	Host                   string   `yaml:"host"`
	Port                   int      `yaml:"port"`
	SSL                    bool     `yaml:"ssl"`
	SSLCert                string   `yaml:"ssl_cert"`
	SSLKey                 string   `yaml:"ssl_key"`
	LogLevel               string   `yaml:"log_level"`
	MainLogFile            string   `yaml:"main_log_file"`
	AccessLog              string   `yaml:"access_log"`
	AccessLogPath          string   `yaml:"access_log_path"`
	SekaiUserJWTSigningKey string   `yaml:"sekai_user_jwt_signing_key,omitempty"`
	EnableTrustProxy       bool     `yaml:"enable_trust_proxy"`
	TrustProxies           []string `yaml:"trust_proxies"`
	ProxyHeader            string   `yaml:"proxy_header"`
}

type ServerConfig struct {
	Enabled                   bool             `yaml:"enabled"`
	MasterDataDir             string           `yaml:"master_data_dir"`
	UseSecondLevelTrackerCron bool             `yaml:"use_second_level_tracker_cron,omitempty"`
	TrackerCron               string           `yaml:"tracker_cron"`
	GormConfig                model.GormConfig `yaml:"gorm_config"`
}

type Config struct {
	Redis    RedisConfig                              `yaml:"redis"`
	Backend  BackendConfig                            `yaml:"backend"`
	Servers  map[model.SekaiServerRegion]ServerConfig `yaml:"servers"`
	SekaiAPI SekaiAPIConfig                           `yaml:"sekai_api"`
}

func init() {
	logger := harukiLogger.NewLogger("ConfigLoader", "DEBUG", nil)
	f, err := os.Open("haruki-tracker-configs.yaml")
	if err != nil {
		logger.Errorf("Failed to open config file: %v", err)
		os.Exit(1)
	}
	defer func(f *os.File) {
		_ = f.Close()
	}(f)

	decoder := yaml.NewDecoder(f)
	if err := decoder.Decode(&Cfg); err != nil {
		logger.Errorf("Failed to parse config: %v", err)
		os.Exit(1)
	}
}
