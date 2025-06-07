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
