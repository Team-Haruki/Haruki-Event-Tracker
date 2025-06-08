from typing import Dict

from enums import SekaiServerRegion
from modules.redis import RedisClient
from modules.sql.engine import DatabaseEngine
from configs import (
    ENABLE_SERVERS,
    DATABASES,
    EVENT_DB_SCHEMA,
    REDIS_HOST,
    REDIS_PORT,
    REDIS_PASSWORD,
    HARUKI_SEKAI_API_ENDPOINT,
)
from modules.tracker.call_api import HarukiSekaiAPIClient

db_engines: Dict[SekaiServerRegion, DatabaseEngine] = {
    server: DatabaseEngine(EVENT_DB_SCHEMA + DATABASES.get(server)) for server, value in ENABLE_SERVERS.items() if value
}
redis_client: RedisClient = RedisClient(REDIS_HOST, REDIS_PORT, REDIS_PASSWORD)
api_client: HarukiSekaiAPIClient = HarukiSekaiAPIClient(HARUKI_SEKAI_API_ENDPOINT)
