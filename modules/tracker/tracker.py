from typing import Optional

from enums import SekaiServerRegion
from modules.redis import RedisClient
from modules.sql.engine import DatabaseEngine
from modules.sql.tables import AbstractWorldLinkTable, get_event_table_class, get_event_names_table_class


class EventTracker:
    def __init__(self, server: SekaiServerRegion, event_id: int, engine: DatabaseEngine, redis: RedisClient):
        self.server = server
        self.event_id = event_id
        self.engine = engine
        self.redis = redis
        self.event_table = get_event_table_class(event_id)
        self.world_link_table: Optional[AbstractWorldLinkTable] = None
        self.event_names_table: get_event_names_table_class(event_id)
