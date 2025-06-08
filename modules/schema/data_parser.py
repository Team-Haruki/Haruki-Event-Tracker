from pydantic import BaseModel
from typing import Optional, Dict, Any

from modules.enums import SekaiServerRegion, SekaiEventType, SekaiEventStatus


class WorldBloomChapterStatus(BaseModel):
    server: SekaiServerRegion
    event_id: int
    character_id: int
    chapter_status: SekaiEventStatus


class EventStatus(BaseModel):
    server: SekaiServerRegion
    event_id: int
    event_type: SekaiEventType
    event_status: SekaiEventStatus
    remain: str
    assetbundle_name: str
    chapter_statuses: Optional[Dict[int, WorldBloomChapterStatus]] = None
    detail: Dict[str, Any]
