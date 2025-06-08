from aiopath import AsyncPath
from typing import Dict, Optional
from modules.enums import SekaiServerRegion

REDIS_HOST: str = "localhost"
REDIS_PORT: int = 6379
REDIS_PASSWORD: Optional[str] = None
HARUKI_SEKAI_API_ENDPOINT: str = "https://event.haruki.example.com/api"
BASE_MASTER_DATA_DIR: AsyncPath = AsyncPath(__file__).parent / "Data" / "master"
ENABLE_SERVERS: Dict[SekaiServerRegion, bool] = {
    SekaiServerRegion.JP: True,
    SekaiServerRegion.EN: False,
    SekaiServerRegion.TW: False,
    SekaiServerRegion.KR: False,
    SekaiServerRegion.CN: False,
}
MASTER_DATA_DIRS: Dict[SekaiServerRegion, Optional[AsyncPath]] = {
    SekaiServerRegion.JP: BASE_MASTER_DATA_DIR / "haruki-sekai-master/master",
    SekaiServerRegion.EN: BASE_MASTER_DATA_DIR / "haruki-sekai-en-master/master",
    SekaiServerRegion.TW: BASE_MASTER_DATA_DIR / "haruki-sekai-tc-master/master",
    SekaiServerRegion.KR: BASE_MASTER_DATA_DIR / "haruki-sekai-kr-master/master",
    SekaiServerRegion.CN: BASE_MASTER_DATA_DIR / "haruki-sekai-sc-master/master",
}
DATABASES: Dict[SekaiServerRegion, str] = {
    SekaiServerRegion.JP: "jp_event_db",
    SekaiServerRegion.EN: "en_event_db",
    SekaiServerRegion.TW: "tw_event_db",
    SekaiServerRegion.KR: "kr_event_db",
    SekaiServerRegion.CN: "cn_event_db",
}
EVENT_DB_SCHEMA = "mysql+aiomysql://{user}:{password}@{host}:{port}/"
