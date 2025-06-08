from enum import Enum


class SekaiServerRegion(Enum):
    JP = "jp"
    EN = "en"
    TW = "tw"
    KR = "kr"
    CN = "cn"


class SekaiEventType(Enum):
    MARATHON = "marathon"  # 马拉松 (普活)
    CHEERFUL_CARNIVAL = "cheerful_carnival"  # 欢乐嘉年华 (5v5)
    WORLD_BLOOM = "world_bloom"  # 世界连接 (World Link)


class SekaiEventStatus(Enum):
    NOT_STARTED = "not_started"  # 还没开始
    ONGOING = "ongoing"  # 正在进行
    AGGREGATING = "aggregating"  # 集算中
    ENDED = "ended"  # 已结束


class SekaiEventSpeedType(Enum):
    HOURLY = "hourly"
    SEMI_DAILY = "semi_daily"
    DAILY = "daily"


class SekaiEventRankingLines(Enum):
    NORMAL = [
        10,
        20,
        30,
        40,
        50,
        100,
        200,
        300,
        400,
        500,
        1000,
        1500,
        2000,
        2500,
        3000,
        4000,
        5000,
        10000,
        20000,
        30000,
        40000,
        50000,
        100000,
        200000,
        300000,
    ]
    WORLD_BLOOM = [
        10,
        20,
        30,
        40,
        50,
        100,
        200,
        300,
        400,
        500,
        1000,
        2000,
        3000,
        4000,
        5000,
        7000,
        10000,
        20000,
        30000,
        40000,
        50000,
        70000,
        100000,
    ]
