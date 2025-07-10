import time
from fastapi import Response
from fastapi.responses import JSONResponse
from typing import List, Dict, Tuple, Union, Optional, Any
from apscheduler.schedulers.asyncio import AsyncIOScheduler

from modules.redis import RedisClient
from modules.enums import SekaiServerRegion
from modules.sql.engine import DatabaseEngine
from modules.sql.helpers import get_user_name_data, fetch_ranking_rows, generate_response
from configs import (
    ENABLE_SERVERS,
    DATABASES,
    EVENT_DB_SCHEMA,
    REDIS_HOST,
    REDIS_PORT,
    REDIS_PASSWORD,
    HARUKI_SEKAI_API_ENDPOINT,
    HARUKI_SEKAI_API_TOKEN,
)
from modules.tracker.call_api import HarukiSekaiAPIClient
from modules.schema.api import (
    UserLatestRankingQueryResponseSchema,
    RankingScoreGrowthSchema,
    RankingLineScoreSchema,
)

scheduler: AsyncIOScheduler = AsyncIOScheduler()
db_engines: Dict[SekaiServerRegion, DatabaseEngine] = {
    server: DatabaseEngine(EVENT_DB_SCHEMA + DATABASES.get(server)) for server, value in ENABLE_SERVERS.items() if value
}
redis_client: RedisClient = RedisClient(REDIS_HOST, REDIS_PORT, REDIS_PASSWORD)
api_client: HarukiSekaiAPIClient = HarukiSekaiAPIClient(HARUKI_SEKAI_API_ENDPOINT, HARUKI_SEKAI_API_TOKEN)


async def get_db_context(
    server: str,
    event_id: int,
    table_getter,
) -> Tuple[DatabaseEngine, Any]:
    server_enum = SekaiServerRegion(server)
    engine = db_engines[server_enum]
    table_cls = table_getter(event_id)
    return engine, table_cls


def build_filters(
    table_cls,
    user_id: Optional[str] = None,
    rank: Optional[int] = None,
    character_id: Optional[int] = None,
    extra_filters: Optional[List] = None,
) -> List:
    filters = []
    if user_id is not None:
        filters.append(table_cls.user_id == user_id)
    if rank is not None:
        filters.append(table_cls.rank == rank)
    if character_id is not None:
        filters.append(table_cls.character_id == character_id)
    if extra_filters:
        filters.extend(extra_filters)
    return filters


async def query_rank_data(
    engine: DatabaseEngine,
    table,
    filters,
    *,
    schema_class,
    latest_only: bool = False,
    include_user: bool = False,
    user_id: Optional[str] = None,
    query_user_only: bool = False,
    event_id: Optional[int] = None,
) -> Response:
    if query_user_only and user_id and event_id is not None:
        user_data = await get_user_name_data(engine, event_id, user_id)
        if user_data:
            response = UserLatestRankingQueryResponseSchema(rank_data=None, user_data=user_data)
            return JSONResponse(content=response.model_dump(), status_code=200)
        return JSONResponse(content={"error": "not found"}, status_code=404)
    else:
        async with engine.session() as session:
            rows = await fetch_ranking_rows(session, table, filters, latest_only)
            user_data = None
            if include_user:
                target_user_id = user_id or (rows[0].user_id if latest_only and rows else None)
                if target_user_id and event_id is not None:
                    user_data = await get_user_name_data(engine, event_id, target_user_id)
            return await generate_response(rows, schema_class, latest_only, user_data)


async def get_ranking_data_by_rankings(
    session,
    table_cls,
    rankings: List[int],
    *,
    extra_filters: Optional[List] = None,
    latest_only: bool = False,
    interval: Optional[int] = None,
) -> List[Union[RankingLineScoreSchema, RankingScoreGrowthSchema]]:
    if extra_filters is None:
        extra_filters = []
    result = []
    now_ts = int(time.time()) if interval is not None else None
    start_ts = now_ts - interval if interval is not None else None

    for rank in rankings:
        filters = [table_cls.rank == rank] + extra_filters
        if start_ts is not None:
            filters.append(table_cls.timestamp >= start_ts)
        rows = await fetch_ranking_rows(session, table_cls, filters=filters, latest_only=latest_only)
        if not rows:
            continue
        if interval is not None and len(rows) >= 2:
            earlier = rows[0]
            latest = rows[-1]
            result.append(
                RankingScoreGrowthSchema(
                    rank=rank,
                    timestamp_latest=latest.timestamp,
                    score_latest=latest.score,
                    timestamp_earlier=earlier.timestamp,
                    score_earlier=earlier.score,
                    growth=latest.score - earlier.score,
                )
            )
        elif latest_only:
            row = rows[0]
            result.append(RankingLineScoreSchema(timestamp=row.timestamp, rank=row.rank, score=row.score))
    return result


async def get_latest_ranking_lines(
    server: str,
    event_id: int,
    table_getter,
    rankings: List[int],
    extra_filters: Optional[List] = None,
) -> List[RankingLineScoreSchema]:
    engine, table_cls = await get_db_context(server, event_id, table_getter)
    async with engine.session() as session:
        return await get_ranking_data_by_rankings(
            session,
            table_cls,
            rankings,
            extra_filters=extra_filters or [],
            latest_only=True,
        )


async def get_ranking_growth_over_interval(
    server: str,
    event_id: int,
    table_getter,
    rankings: List[int],
    interval: int,
    extra_filters: Optional[List] = None,
) -> List[RankingScoreGrowthSchema]:
    engine, table_cls = await get_db_context(server, event_id, table_getter)
    async with engine.session() as session:
        return await get_ranking_data_by_rankings(
            session,
            table_cls,
            rankings,
            extra_filters=extra_filters or [],
            interval=interval,
        )
