from typing import List
from redis.asyncio import Redis
from fastapi_cache import FastAPICache
from fastapi_cache.decorator import cache
from contextlib import asynccontextmanager
from fastapi import FastAPI, Response, APIRouter
from apscheduler.triggers.cron import CronTrigger
from fastapi_cache.backends.redis import RedisBackend

from event_tracker import track_event_data
from modules.enums import SekaiEventRankingLines
from event_tracker import logger as event_tracker_logger
from modules.cache_helpers import ORJsonCoder, cache_key_builder
from modules.sql.tables import get_event_table_class, get_world_bloom_table_class, get_event_names_table_class
from modules.schema.api import (
    RecordedRankingSchema,
    RecordedWorldBloomRankingSchema,
    RecordedUserNameSchema,
    UserLatestRankingQueryResponseSchema,
    UserAllRankingDataQueryResponseSchema,
    RankingLineScoreSchema,
    RankingScoreGrowthSchema,
)
from utils import (
    scheduler,
    db_engines,
    api_client,
    build_filters,
    get_db_context,
    query_rank_data,
    get_ranking_growth_over_interval,
    get_latest_ranking_lines,
)
from configs import REDIS_HOST, REDIS_PORT, REDIS_PASSWORD

@asynccontextmanager
async def lifespan(_app: FastAPI):
    redis_client = Redis(
        host=REDIS_HOST, port=REDIS_PORT, password=REDIS_PASSWORD, decode_responses=False, encoding="utf-8"
    )
    FastAPICache.init(RedisBackend(redis_client), prefix="fastapi-cache")
    await api_client.init()
    await event_tracker_logger.start()
    for db_engine in db_engines:
        await db_engines[db_engine].init_engine()
    scheduler.add_job(track_event_data, CronTrigger(second=1))
    scheduler.start()
    yield
    await api_client.close()
    await event_tracker_logger.stop()
    for db_engine in db_engines:
        await db_engines[db_engine].shutdown_engine()
    scheduler.shutdown()


api = APIRouter(prefix="/event/{server}/{event_id}")


@api.get(
    "/latest-ranking/user/{user_id}",
    response_model=UserLatestRankingQueryResponseSchema,
    summary="获取指定活动指定玩家最新排名数据",
    description="返回指定玩家在指定活动中的最新一条排名记录及玩家信息",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_normal_ranking_by_user_id(server: str, event_id: int, user_id: str) -> Response:
    engine, table_cls = await get_db_context(server, event_id, get_event_table_class)
    return await query_rank_data(
        engine,
        table_cls,
        filters=build_filters(table_cls, user_id=user_id),
        schema_class=RecordedRankingSchema,
        latest_only=True,
        include_user=True,
        user_id=user_id,
        query_user_only=False,
        event_id=event_id,
    )


@api.get(
    "/latest-ranking/rank/{rank}",
    response_model=UserLatestRankingQueryResponseSchema,
    summary="获取指定活动指定排名最新排名数据",
    description="返回指定活动中指定排名的最新一条排名记录及玩家信息",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_normal_ranking_by_rank(server: str, event_id: int, rank: int) -> Response:
    engine, table_cls = await get_db_context(server, event_id, get_event_table_class)
    return await query_rank_data(
        engine,
        table_cls,
        filters=build_filters(table_cls, rank=rank),
        schema_class=RecordedRankingSchema,
        latest_only=True,
        include_user=True,
        user_id=None,
        query_user_only=False,
        event_id=event_id,
    )


@api.get(
    "/latest-world-bloom-ranking/character/{character_id}/user/{user_id}",
    response_model=UserLatestRankingQueryResponseSchema,
    summary="获取指定玩家指定World Link活动指定角色单榜最新排名数据",
    description="返回指定玩家在指定World Link活动指定角色单榜中的最新一条排名记录及玩家信息",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_world_bloom_ranking_by_user_id(server: str, event_id: int, character_id: int, user_id: str) -> Response:
    engine, table_cls = await get_db_context(server, event_id, get_world_bloom_table_class)
    return await query_rank_data(
        engine,
        table_cls,
        filters=build_filters(table_cls, user_id=user_id, character_id=character_id),
        schema_class=RecordedWorldBloomRankingSchema,
        latest_only=True,
        include_user=True,
        user_id=user_id,
        query_user_only=False,
        event_id=event_id,
    )


@api.get(
    "/latest-world-bloom-ranking/character/{character_id}/rank/{rank}",
    response_model=UserLatestRankingQueryResponseSchema,
    summary="获取指定排名指定World Link活动指定角色单榜最新排名数据",
    description="返回指定排名在指定World Link活动指定角色单榜中的最新一条排名记录及玩家信息",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_world_bloom_ranking_by_rank(server: str, event_id: int, character_id: int, rank: int) -> Response:
    engine, table_cls = await get_db_context(server, event_id, get_world_bloom_table_class)
    return await query_rank_data(
        engine,
        table_cls,
        filters=build_filters(table_cls, rank=rank, character_id=character_id),
        schema_class=RecordedWorldBloomRankingSchema,
        latest_only=True,
        include_user=False,
        user_id=None,
        query_user_only=False,
        event_id=event_id,
    )


@api.get(
    "/trace-ranking/user/{user_id}",
    response_model=UserAllRankingDataQueryResponseSchema,
    summary="获取指定活动指定玩家的所有已记录的排名数据",
    description="返回指定玩家在指定活动中的所有已记录的排名数据（时间升序）及玩家信息",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_all_normal_ranking_by_user_id(server: str, event_id: int, user_id: str) -> Response:
    engine, table_cls = await get_db_context(server, event_id, get_event_table_class)
    return await query_rank_data(
        engine,
        table_cls,
        filters=build_filters(table_cls, user_id=user_id),
        schema_class=RecordedRankingSchema,
        latest_only=False,
        include_user=True,
        user_id=user_id,
        query_user_only=False,
        event_id=event_id,
    )


@api.get(
    "/trace-ranking/rank/{rank}",
    response_model=UserAllRankingDataQueryResponseSchema,
    summary="获取指定活动指定排名的所有已记录的排名数据",
    description="返回指定排名在指定活动中的所有已记录的排名数据（时间升序）",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_all_normal_ranking_by_rank(server: str, event_id: int, rank: int) -> Response:
    engine, table_cls = await get_db_context(server, event_id, get_event_table_class)
    return await query_rank_data(
        engine,
        table_cls,
        filters=build_filters(table_cls, rank=rank),
        schema_class=RecordedRankingSchema,
        latest_only=False,
        include_user=False,
        user_id=None,
        query_user_only=False,
        event_id=event_id,
    )


@api.get(
    "/trace-world-bloom-ranking/character/{character_id}/user/{user_id}",
    response_model=UserAllRankingDataQueryResponseSchema,
    summary="获取指定玩家指定World Link活动指定角色单榜的所有已记录的排名数据",
    description="返回指定玩家在指定World Link活动指定角色单榜中的所有已记录的排名数据（时间升序）及玩家信息",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_all_world_bloom_ranking_by_user_id(
    server: str, event_id: int, character_id: int, user_id: str
) -> Response:
    engine, table_cls = await get_db_context(server, event_id, get_world_bloom_table_class)
    return await query_rank_data(
        engine,
        table_cls,
        filters=build_filters(table_cls, user_id=user_id, character_id=character_id),
        schema_class=RecordedWorldBloomRankingSchema,
        latest_only=False,
        include_user=True,
        user_id=user_id,
        query_user_only=False,
        event_id=event_id,
    )


@api.get(
    "/trace-world-bloom-ranking/character/{character_id}/rank/{rank}",
    response_model=UserAllRankingDataQueryResponseSchema,
    summary="获取指定排名指定World Link活动指定角色单榜的所有已记录的排名数据",
    description="返回指定排名在指定World Link活动指定角色单榜中的所有已记录的排名数据（时间升序）",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_all_world_bloom_ranking_by_rank(server: str, event_id: int, character_id: int, rank: int) -> Response:
    engine, table_cls = await get_db_context(server, event_id, get_world_bloom_table_class)
    return await query_rank_data(
        engine,
        table_cls,
        filters=build_filters(table_cls, rank=rank, character_id=character_id),
        schema_class=RecordedWorldBloomRankingSchema,
        latest_only=False,
        include_user=False,
        user_id=None,
        query_user_only=False,
        event_id=event_id,
    )


@api.get(
    "/user-data/{user_id}",
    response_model=RecordedUserNameSchema,
    summary="获取指定用户的基础信息",
    description="返回指定用户的用户名与欢乐嘉年华(5v5)队伍ID",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_user_data_by_user_id(server: str, event_id: int, user_id: str) -> Response:
    engine, table_cls = await get_db_context(server, event_id, get_event_names_table_class)
    return await query_rank_data(
        engine,
        table_cls,
        filters=build_filters(table_cls),
        schema_class=RecordedUserNameSchema,
        latest_only=False,
        include_user=False,
        user_id=user_id,
        query_user_only=True,
        event_id=event_id,
    )


@api.get(
    "/ranking-lines",
    response_model=List[RankingLineScoreSchema],
    summary="获取指定活动最新分数线",
    description="根据指定活动获取所有固定排名的最新分数",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_ranking_lines(server: str, event_id: int) -> List[RankingLineScoreSchema]:
    return await get_latest_ranking_lines(
        server,
        event_id,
        get_event_table_class,
        SekaiEventRankingLines.NORMAL.value,
    )


@api.get(
    "/ranking-score-growth/interval/{interval}",
    response_model=List[RankingScoreGrowthSchema],
    summary="获取指定活动排名的分数增长速度",
    description="根据指定活动获取所有固定排名在特定时间内的分数增长速度",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_ranking_score_growths(server: str, event_id: int, interval: int) -> List[RankingScoreGrowthSchema]:
    return await get_ranking_growth_over_interval(
        server,
        event_id,
        get_event_table_class,
        SekaiEventRankingLines.NORMAL.value,
        interval,
    )


@api.get(
    "/world-bloom-ranking-lines/character/{character_id}",
    response_model=List[RankingLineScoreSchema],
    summary="获取指定World Link活动指定角色单榜排名最新分数线",
    description="根据指定World Link活动指定角色单榜获取所有固定排名的最新分数",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_world_bloom_ranking_lines(server: str, event_id: int, character_id: int) -> List[RankingLineScoreSchema]:
    return await get_latest_ranking_lines(
        server,
        event_id,
        get_world_bloom_table_class,
        SekaiEventRankingLines.WORLD_BLOOM.value,
        extra_filters=[get_world_bloom_table_class(event_id).character_id == character_id],
    )


@api.get(
    "/world-bloom-ranking-score-growth/character/{character_id}/interval/{interval}",
    response_model=List[RankingScoreGrowthSchema],
    summary="获取指定World Link活动指定角色单榜排名的分数增长速度",
    description="根据指定World Link活动指定角色单榜获取所有固定排名在特定时间内的分数增长速度",
)
@cache(expire=60, namespace="event_ranking", coder=ORJsonCoder, key_builder=cache_key_builder) # type: ignore
async def get_world_bloom_ranking_score_growths(
    server: str, event_id: int, character_id: int, interval: int
) -> List[RankingScoreGrowthSchema]:
    return await get_ranking_growth_over_interval(
        server,
        event_id,
        get_world_bloom_table_class,
        SekaiEventRankingLines.WORLD_BLOOM.value,
        interval,
        extra_filters=[get_world_bloom_table_class(event_id).character_id == character_id],
    )


app = FastAPI(lifespan=lifespan)
app.include_router(api)
