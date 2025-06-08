import asyncio
from typing import Optional, Dict

from configs import ENABLE_SERVERS, MASTER_DATA_DIRS
from modules.enums import SekaiServerRegion, SekaiEventStatus, SekaiEventType
from utils import db_engines, redis_client, api_client
from modules.logger import AsyncLogger
from modules.tracker.data_parser import EventDataParser
from modules.tracker.tracker import EventTracker

logger = AsyncLogger(__name__, level="DEBUG")
trackers: Dict[SekaiServerRegion, Optional[EventTracker]] = {
    server: None for server, value in ENABLE_SERVERS.items() if value
}
data_parsers: Dict[SekaiServerRegion, Optional[EventDataParser]] = {
    server: EventDataParser(server, MASTER_DATA_DIRS.get(server)) for server, value in ENABLE_SERVERS.items() if value
}


async def track_ranking_data(server: SekaiServerRegion, tracker: Optional[EventTracker] = None) -> None:
    await logger.info(f"Start tracking event ranking data for {server.value.upper()} server...")
    current_event_data = await data_parsers.get(server).get_current_event_status()
    if not current_event_data:
        await logger.warning(f"{server.value.upper()} server event tracker didn't detect new event. skipped.")
    elif not trackers.get(server) or tracker.event_id < current_event_data.event_id:
        if tracker:
            await tracker.logger.stop()
        await logger.info(f"Creating new event tracker for {server.value.upper()} server...")
        tracker = EventTracker(
            server,
            current_event_data.event_id,
            current_event_data.event_type,
            db_engines.get(server),
            redis_client,
            api_client,
            current_event_data.chapter_statuses if current_event_data.chapter_statuses else None,
        )
        trackers[server] = tracker
        await trackers[server].init()
        await logger.info(f"Created new event tracker for {server.value.upper()} server.")
    if current_event_data.event_id == tracker.event_id:
        if tracker.is_event_ended:
            await logger.info(
                f"{server.value.upper()} server detected event tracker detected event is ended, skipped tracking."
            )
            return None
        elif current_event_data.event_status == SekaiEventStatus.ENDED and not tracker.is_event_ended:
            await logger.info(
                f"{server.value.upper()} server event tracker detected event is ended, finishing tracking..."
            )
            await tracker.record_ranking_data_concurrently()
            tracker.is_event_ended = True
            return None
        elif current_event_data.event_status == SekaiEventStatus.AGGREGATING:
            await logger.info(
                f"{server.value.upper()} server event tracker detected event is aggregating, skipped tracking..."
            )
            return None
        elif current_event_data.event_type == SekaiEventType.WORLD_BLOOM:
            for character, detail in current_event_data.chapter_statuses.items():
                if detail.chapter_status == SekaiEventStatus.NOT_STARTED:
                    continue
                elif detail.chapter_status == SekaiEventStatus.ENDED and not tracker.is_world_bloom_chapter_ended.get(
                    character
                ):
                    await logger.info(
                        f"{server.value.upper()} server event tracker detected world bloom character "
                        f"{character}'s chapter is ended, finishing tracking..."
                    )
                    await tracker.record_ranking_data_concurrently(is_only_record_world_bloom=True)
                    tracker.is_world_bloom_chapter_ended.update({character: True})
                    break
                elif detail.chapter_status == SekaiEventStatus.AGGREGATING:
                    await logger.info(
                        f"{server.value.upper()} server event tracker detected world bloom "
                        f"event character {character}'s chapter is aggregating, skipped tracking."
                    )
                    continue
    await logger.info(f"{server.value.upper()} server event tracker started recording ranking data...")
    await tracker.record_ranking_data_concurrently()
    await logger.info(f"{server.value.upper()} server event tracker finished recording ranking data.")
    return None


async def track_event_data() -> None:
    for server, tracker in trackers.items():
        asyncio.create_task(track_ranking_data(server, tracker))
