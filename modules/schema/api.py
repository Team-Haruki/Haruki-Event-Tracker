from pydantic import BaseModel
from typing import Optional, Union, List


class RecordedRankingSchema(BaseModel):
    timestamp: int
    user_id: str
    score: int
    rank: int

    model_config = {"from_attributes": True}


class RecordedWorldBloomRankingSchema(RecordedRankingSchema):
    character_id: Optional[int] = None


class RecordedUserNameSchema(BaseModel):
    user_id: str
    name: str
    cheerful_team_id: Optional[int] = None

    model_config = {"from_attributes": True}


class UserLatestRankingQueryResponseSchema(BaseModel):
    rank_data: Optional[Union[RecordedRankingSchema, RecordedWorldBloomRankingSchema]] = None
    user_data: Optional[RecordedUserNameSchema] = None


class UserAllRankingDataQueryResponseSchema(BaseModel):
    rank_data: Optional[List[Union[RecordedRankingSchema, RecordedWorldBloomRankingSchema]]] = None
    user_data: Optional[RecordedUserNameSchema] = None


class RankingLineScoreSchema(BaseModel):
    rank: int
    score: int
    timestamp: int


class RankingScoreGrowthSchema(BaseModel):
    rank: int
    timestamp_latest: int
    score_latest: int
    timestamp_earlier: Optional[int] = None
    score_earlier: Optional[int] = None
    growth: Optional[int] = None
