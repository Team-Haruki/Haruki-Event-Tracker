package model

type GormLoggerConfig struct {
	Level                     string `yaml:"level"`
	SlowThreshold             string `yaml:"slow_threshold,omitempty"`
	IgnoreRecordNotFoundError bool   `yaml:"ignore_record_not_found_error,omitempty"`
	Colorful                  bool   `yaml:"colorful,omitempty"`
}

type GormNamingConfig struct {
	TablePrefix   string `yaml:"table_prefix,omitempty"`
	SingularTable bool   `yaml:"singular_table,omitempty"`
}

type GormConfig struct {
	Dialect                                  string           `yaml:"dialect"`
	DSN                                      string           `yaml:"dsn"`
	MaxOpenConns                             int              `yaml:"max_open_conns,omitempty"`
	MaxIdleConns                             int              `yaml:"max_idle_conns,omitempty"`
	ConnMaxLifetime                          string           `yaml:"conn_max_lifetime,omitempty"`
	PrepareStmt                              bool             `yaml:"prepare_stmt,omitempty"`
	DisableForeignKeyConstraintWhenMigrating bool             `yaml:"disable_fk_migrate,omitempty"`
	Logger                                   GormLoggerConfig `yaml:"logger"`
	Naming                                   GormNamingConfig `yaml:"naming"`
}
