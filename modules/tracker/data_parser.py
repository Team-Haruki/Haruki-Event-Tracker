import time
import orjson
import hashlib
from pathlib import Path
from aiopath import AsyncPath
from typing import Union, Dict, List, Any, Optional

from enums import SekaiServerRegion, SekaiEventStatus, SekaiEventType
from modules.schema.data_parser import WorldBloomChapterStatus, EventStatus


class EventDataParser:
    def __init__(self, server: SekaiServerRegion, master_dir: Union[str, Path, AsyncPath]):
        self.server = server
        self.master_dir = AsyncPath(master_dir)
        self.cached_data = {}
        self.cached_data_hash = {}

    @staticmethod
    def compute_hash(data: bytes) -> str:
        return hashlib.sha256(data).hexdigest()

    @staticmethod
    def event_time_remain(remain_time, second=True, server: SekaiServerRegion = SekaiServerRegion.JP):
        translations = {
            SekaiServerRegion.JP: {"second": "秒", "minute": "分", "hour": "小时", "day": "天"},
            SekaiServerRegion.CN: {"second": "秒", "minute": "分", "hour": "小时", "day": "天"},
            SekaiServerRegion.TW: {"second": "秒", "minute": "分", "hour": "小時", "day": "天"},
            SekaiServerRegion.EN: {"second": "s", "minute": "m", "hour": "h", "day": "d"},
            SekaiServerRegion.KR: {"second": "초", "minute": "분", "hour": "시간", "day": "일"},
        }

        t = translations.get(server, translations[SekaiServerRegion.JP])

        if remain_time < 60:
            return f"{int(remain_time)}{t['second']}" if second else f"0{t['minute']}"
        elif remain_time < 60 * 60:
            return (
                f"{int(remain_time / 60)}{t['minute']}{int(remain_time % 60)}{t['second']}"
                if second
                else f"{int(remain_time / 60)}{t['minute']}"
            )
        elif remain_time < 60 * 60 * 24:
            hours = int(remain_time / 60 / 60)
            remain = remain_time - 3600 * hours
            return (
                f"{hours}{t['hour']}{int(remain / 60)}{t['minute']}{int(remain % 60)}{t['second']}"
                if second
                else f"{hours}{t['hour']}{int(remain / 60)}{t['minute']}"
            )
        else:
            days = int(remain_time / 3600 / 24)
            remain = remain_time - 3600 * 24 * days
            return (
                f"{days}{t['day']}{EventDataParser.event_time_remain(remain, second=True, server=server)}"
                if second
                else f"{days}{t['day']}{EventDataParser.event_time_remain(remain, second=False, server=server)}"
            )

    async def load_data(self, path: Union[str, Path, AsyncPath]) -> Union[Dict[str, Any], List[Dict[str, Any]]]:
        path = AsyncPath(path)
        key = str(path)
        if key in self.cached_data:
            raw_data = await path.read_bytes()
            current_hash = self.compute_hash(raw_data)
            if self.cached_data_hash.get(key) == current_hash:
                return self.cached_data[key]
            else:
                parsed = orjson.loads(raw_data)
                self.cached_data[key] = parsed
                self.cached_data_hash[key] = current_hash
                return parsed
        else:
            raw_data = await path.read_bytes()
            parsed = orjson.loads(raw_data)
            self.cached_data[key] = parsed
            self.cached_data_hash[key] = self.compute_hash(raw_data)
            return parsed

    async def load_event_data(self) -> Union[Dict[str, Any], List[Dict[str, Any]]]:
        return await self.load_data(AsyncPath(self.master_dir / "events.json"))

    async def load_world_link_chapter_data(self) -> Union[Dict[str, Any], List[Dict[str, Any]]]:
        return await self.load_data(AsyncPath(self.master_dir / "worldBlooms.json"))

    async def get_world_bloom_character_statuses(self, event_id: int) -> List[WorldBloomChapterStatus]:
        data = await self.load_world_link_chapter_data()
        now = int(round(time.time() * 1000))
        result = []
        for chapter in data:
            character_id = chapter["characterId"]
            if chapter["eventId"] == event_id:
                if chapter["chapterEndAt"] <= now:
                    chapter_status = SekaiEventStatus.ENDED
                elif chapter["aggregateAt"] < now < chapter["chapterEndAt"]:
                    chapter_status = SekaiEventStatus.AGGREGATING
                elif chapter["chapterStartAt"] < now < chapter["aggregateAt"]:
                    chapter_status = SekaiEventStatus.ONGOING
                else:
                    chapter_status = SekaiEventStatus.NOT_STARTED
                result.append(
                    WorldBloomChapterStatus(
                        server=self.server, event_id=event_id, character_id=character_id, chapter_status=chapter_status
                    )
                )
        return result

    async def get_current_event_status(self) -> Optional[EventStatus]:
        data = await self.load_event_data()
        for i in range(0, len(data)):
            start_at = data[i]["startAt"]
            end_at = data[i]["closedAt"]
            assetbundle_name = data[i]["assetbundleName"]
            now = int(round(time.time() * 1000))
            remain = ""
            if not start_at < now < end_at:
                continue
            event_id = data[i]["id"]
            event_type = SekaiEventType[data[i]["eventType"]]
            if data[i]["startAt"] < now < data[i]["aggregateAt"]:
                status = SekaiEventStatus.ONGOING
                remain = self.event_time_remain(remain_time=(data[i]["aggregateAt"] - now) / 1000, server=self.server)
            elif data[i]["aggregateAt"] < now < data[i]["aggregateAt"] + 600000:
                status = SekaiEventStatus.AGGREGATING
            else:
                status = SekaiEventStatus.ENDED
            return EventStatus(
                server=self.server,
                event_id=event_id,
                event_type=event_type,
                event_status=status,
                remain=remain,
                assetbundle_name=assetbundle_name,
                chapter_statuses=await self.get_world_bloom_character_statuses(event_id=event_id)
                if event_type == SekaiEventType.WORLD_BLOOM
                else None,
                detail=data[i],
            )
        return None
