from typing import List, Optional

from pydantic import BaseModel
from .call_api import PlayerRankingSchema


class RecordPlayerRankingSchemaBase(BaseModel):
    timestamp: int
    user_id: int
    score: int
    rank: int


class RecordChapterPlayerRankingSchema(RecordPlayerRankingSchemaBase):
    character_id: int


class RecordPlayerNameSchema(BaseModel):
    user_id: int
    name: str
    cheerful_team_id: Optional[int] = None


class HandledRankingDataSchema(BaseModel):
    record_time: int
    rankings: List[PlayerRankingSchema]
    character_id: Optional[int] = None
    world_bloom_rankings: Optional[List[PlayerRankingSchema]] = None
