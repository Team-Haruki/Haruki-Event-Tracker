import time
from sqlalchemy import insert
from typing import Optional, Dict, List, Type
from sqlalchemy.dialects.mysql import insert as mysql_insert
from sqlalchemy.dialects.sqlite import insert as sqlite_insert

from modules.enums import SekaiServerRegion, SekaiEventType, SekaiEventStatus
from modules.logger import AsyncLogger
from modules.redis import RedisClient
from modules.schema.tracker import (
    HandledRankingDataSchema,
    RecordPlayerRankingSchemaBase,
    RecordChapterPlayerRankingSchema,
    RecordPlayerNameSchema,
)
from modules.schema.data_parser import WorldBloomChapterStatus
from modules.schema.call_api import PlayerRankingSchema
from modules.sql.engine import DatabaseEngine
from modules.sql.tables import (
    AbstractEventTable,
    AbstractWorldBloomTable,
    AbstractEventNamesTable,
    get_world_bloom_table_class,
    get_event_table_class,
    get_event_names_table_class,
)
from modules.tracker.call_api import HarukiSekaiAPIClient


class EventTracker:
    def __init__(
        self,
        server: SekaiServerRegion,
        event_id: int,
        event_type: SekaiEventType,
        engine: DatabaseEngine,
        redis: RedisClient,
        api_client: HarukiSekaiAPIClient,
        world_bloom_statuses: Optional[Dict[int, WorldBloomChapterStatus]] = None,
    ) -> None:
        self.logger = AsyncLogger(__name__, level="DEBUG")
        self.server: SekaiServerRegion = server
        self.event_id: int = event_id
        self.event_type: SekaiEventType = event_type
        self.is_event_ended: bool = False
        self.world_bloom_statuses: Optional[Dict[int, WorldBloomChapterStatus]] = (
            world_bloom_statuses if event_type == SekaiEventType.WORLD_BLOOM else None
        )
        self.engine: DatabaseEngine = engine
        self.redis: RedisClient = redis
        self.api_client: HarukiSekaiAPIClient = api_client
        self.event_table: Type[AbstractEventTable] = get_event_table_class(event_id)
        self.world_bloom_table: Optional[Type[AbstractWorldBloomTable]] = (
            get_world_bloom_table_class(event_id) if event_type == SekaiEventType.WORLD_BLOOM else None
        )
        self.event_names_table: Type[AbstractEventNamesTable] = get_event_names_table_class(event_id)
        if event_type == SekaiEventType.WORLD_BLOOM and world_bloom_statuses:
            self.is_world_bloom_chapter_ended = {character_id: False for character_id in world_bloom_statuses}
        else:
            self.is_world_bloom_chapter_ended = None

    async def init(self) -> None:
        await self.logger.start()
        await self.logger.info(f"Initializing {self.server.value.upper()} {self.event_id} event tracker...")
        tables = [self.event_table, self.world_bloom_table, self.event_names_table]
        await self.engine.create_tables(tables)
        await self.logger.info(f"Initialized {self.server.value.upper()} {self.event_id} event tracker.")

    async def detect_cache(self, key: str, new_data: List) -> bool:
        cached_data = await self.redis.get(key) or []
        if cached_data != new_data:
            await self.redis.set(key, new_data)
            return False
        else:
            return True

    async def merge_rankings(
        self, top100_rankings: List[PlayerRankingSchema], border_rankings: List[PlayerRankingSchema], cache_key: str
    ) -> List[PlayerRankingSchema]:
        return (
            top100_rankings + [item for item in border_rankings if item.rank != 100]
            if not await self.detect_cache(cache_key, [data.model_dump() for data in border_rankings])
            else top100_rankings
        )

    async def handle_ranking_data(self) -> Optional[HandledRankingDataSchema]:
        top100 = await self.api_client.get_top100(self.event_id, self.server)
        border = await self.api_client.get_border(self.event_id, self.server)

        if not top100:
            await self.logger.warning("It seems that Haruki Sekai API occurred error. skipping tracking...")

        current_time = int(time.time())
        main_top100_rankings = top100.rankings
        main_border_rankings = border.borderRankings
        character_id, wl_top100_rankings, wl_border_rankings = None, None, None

        if self.event_type == SekaiEventType.WORLD_BLOOM:
            for character in top100.userWorldBloomChapterRankings:
                if (
                    self.world_bloom_statuses.get(character.gameCharacterId).chapter_status == SekaiEventStatus.ENDED
                    and not self.is_world_bloom_chapter_ended.get(character.gameCharacterId)
                ) or (
                    self.world_bloom_statuses.get(character.gameCharacterId).chapter_status == SekaiEventStatus.ONGOING
                ):
                    character_id = character.gameCharacterId
                    wl_top100_rankings = character.rankings
                    break

            for character in border.userWorldBloomChapterRankings:
                if character.gameCharacterId != character_id:
                    continue
                elif character.gameCharacterId == character_id:
                    wl_border_rankings = character.rankings
                    break

            wl_rankings = await self.merge_rankings(
                wl_top100_rankings,
                wl_border_rankings,
                f"{self.server.value}_event_{self.event_id}_character_{character_id}_border_cache",
            )
        else:
            wl_rankings = None

        rankings = await self.merge_rankings(
            main_top100_rankings, main_border_rankings, f"{self.server.value}_event_{self.event_id}_border_cache"
        )

        return HandledRankingDataSchema(
            record_time=current_time, rankings=rankings, world_bloom_rankings=wl_rankings, character_id=character_id
        )

    async def record_ranking_data_concurrently(self, is_only_record_world_bloom: bool = False) -> None:
        data = await self.handle_ranking_data()
        if not data:
            return
        rankings = data.rankings or []
        world_bloom_rankings = data.world_bloom_rankings or []
        character_id = data.character_id
        event_rows = [
            RecordPlayerRankingSchemaBase(
                timestamp=data.record_time, user_id=r.userId, score=r.score, rank=r.rank
            ).model_dump()
            for r in rankings
        ]
        wl_rows = []
        if self.world_bloom_table and world_bloom_rankings:
            wl_rows = [
                RecordChapterPlayerRankingSchema(
                    timestamp=data.record_time, user_id=r.userId, score=r.score, rank=r.rank, character_id=character_id
                ).model_dump()
                for r in world_bloom_rankings
            ]
        seen_ids = set()
        name_rows = [
            RecordPlayerNameSchema(
                user_id=r.userId,
                name=r.name,
                cheerful_team_id=r.userCheerfulCarnival.cheerfulCarnivalTeamId if r.userCheerfulCarnival else None,
            ).model_dump()
            for r in rankings + (world_bloom_rankings if world_bloom_rankings else [])
            if r.userId not in seen_ids and not seen_ids.add(r.userId)
        ]
        async with self.engine.session() as session:
            await self.logger.info(f"{self.server.value.upper()} server tracker started inserting ranking data...")
            if not is_only_record_world_bloom:
                await session.execute(insert(self.event_table), event_rows)
            if self.world_bloom_table and wl_rows:
                await session.execute(insert(self.world_bloom_table), wl_rows)
            if session.bind.dialect.name == "mysql":
                stmt = mysql_insert(self.event_names_table).values(name_rows)
                stmt = stmt.on_duplicate_key_update(
                    name=stmt.inserted.name, cheerful_team_id=stmt.inserted.cheerful_team_id
                )
            elif session.bind.dialect.name == "sqlite":
                stmt = sqlite_insert(self.event_names_table).values(name_rows)
                stmt = stmt.on_conflict_do_update(
                    index_elements=["user_id"],
                    set_={"name": stmt.excluded.name, "cheerful_team_id": stmt.excluded.cheerful_team_id},
                )
            await session.execute(stmt)
            await session.commit()
            await self.logger.info(f"{self.server.value.upper()} server tracker finished inserting ranking data.")

        await self.redis.clear_fastapi_cache("event_ranking")