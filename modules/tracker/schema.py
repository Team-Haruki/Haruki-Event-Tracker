from pydantic import BaseModel
from typing import Optional, List, Dict, Any

from enums import SekaiServerRegion, SekaiEventType, SekaiEventStatus


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
    chapter_statuses: Optional[List[WorldBloomChapterStatus]] = None
    detail: Dict[str, Any]
