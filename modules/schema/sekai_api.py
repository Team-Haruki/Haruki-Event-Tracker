from pydantic import BaseModel, field_validator
from typing import Optional, List, Any, Literal


class UserCard(BaseModel):
    model_config = {"extra": "ignore"}
    cardId: Optional[int] = None
    defaultImage: Optional[Literal["original", "special_training"]] = None
    level: Optional[int] = None
    masterRank: Optional[int] = None
    specialTrainingStatus: Optional[Literal["not_doing", "done"]] = None


class UserCheerfulCarnival(BaseModel):
    model_config = {"extra": "ignore"}
    cheerfulCarnivalTeamId: Optional[int] = None
    eventId: Optional[int] = None
    registerAt: Optional[int] = None
    teamChangeCount: Optional[int] = None


class UserProfile(BaseModel):
    model_config = {"extra": "ignore"}
    profileImageType: Optional[str] = None
    twitterId: Optional[str] = None
    userId: Optional[int] = None
    word: Optional[str] = None


class UserProfileHonor(BaseModel):
    model_config = {"extra": "ignore"}
    bondsHonorViewType: Optional[
        Literal["none", "normal", "reverse", "normal_unit_virtual_singer", "reverse_unit_virtual_singer"]
    ] = None
    bondsHonorWordId: Optional[int] = None
    honorId: Optional[int] = None
    honorLevel: Optional[int] = None
    profileHonorType: Optional[
        Literal[
            "normal",
            "bonds",
        ]
    ] = None
    seq: Optional[int] = None


class PlayerRankingSchema(BaseModel):
    model_config = {"extra": "ignore"}
    isOwn: Optional[bool] = None
    name: Optional[str] = None
    rank: Optional[int] = None
    score: Optional[int] = None
    userCard: Optional[UserCard] = None
    userCheerfulCarnival: Optional[UserCheerfulCarnival] = None
    userHonorMissions: Optional[List[Any]] = None
    userId: Optional[int] = None
    userProfile: Optional[UserProfile] = None
    userProfileHonors: Optional[List[UserProfileHonor]] = None

    @field_validator("userCheerfulCarnival", mode="before")
    @classmethod
    def handle_empty_user_cheerful_carnival(cls, v: Any) -> Any:
        if isinstance(v, dict) and not v:
            return None
        return v

    @field_validator("userProfileHonors", mode="before")
    @classmethod
    def handle_empty_user_profile_honors(cls, v: Any) -> Any:
        if isinstance(v, list) and not v:
            return None
        return v


class UserWorldBloomChapterRankingBase(BaseModel):
    model_config = {"extra": "ignore"}
    eventId: Optional[int] = None
    gameCharacterId: Optional[int] = None
    isWorldBloomChapterAggregate: Optional[bool] = None


class UserWorldBloomChapterRanking(UserWorldBloomChapterRankingBase):
    rankings: Optional[List[PlayerRankingSchema]] = None


class UserWorldBloomChapterRankingBorder(UserWorldBloomChapterRankingBase):
    borderRankings: Optional[List[PlayerRankingSchema]] = None


class Top100RankingResponse(BaseModel):
    model_config = {"extra": "ignore"}
    isEventAggregate: Optional[bool] = None
    rankings: Optional[List[PlayerRankingSchema]] = None
    userRankingStatus: Literal["normal", "event_aggregate", "world_aggregate"] = "normal"
    userWorldBloomChapterRankings: Optional[List[UserWorldBloomChapterRanking]] = None


class BorderRankingResponse(BaseModel):
    model_config = {"extra": "ignore"}
    eventId: Optional[int] = None
    isEventAggregate: Optional[bool] = None
    borderRankings: Optional[List[PlayerRankingSchema]] = None
    userWorldBloomChapterRankingBorders: Optional[List[UserWorldBloomChapterRankingBorder]] = None
