from typing import Type, cast
from sqlalchemy import Column, Integer, String, BigInteger

from .base import Base

_event_table_class_cache = {}
_world_bloom_table_class_cache = {}
_event_names_table_class_cache = {}


class AbstractEventTable(Base):
    __abstract__ = True
    timestamp = Column(BigInteger, primary_key=True)
    user_id = Column(String(30), primary_key=True)
    score = Column(Integer, nullable=False)
    rank = Column(Integer, nullable=False)


class AbstractWorldBloomTable(Base):
    __abstract__ = True
    timestamp = Column(BigInteger, primary_key=True)
    user_id = Column(String(30), primary_key=True)
    character_id = Column(Integer, primary_key=True)
    score = Column(Integer, nullable=False)
    rank = Column(Integer, nullable=False)


class AbstractEventNamesTable(Base):
    __abstract__ = True
    user_id = Column(String(30), primary_key=True)
    name = Column(String(300), nullable=False)
    cheerful_team_id = Column(Integer, nullable=True)


def get_event_table_class(event_id: int) -> Type[AbstractEventTable]:
    if event_id in _event_table_class_cache:
        return _event_table_class_cache[event_id]
    tablename = f"event_{event_id}"
    cls = type(
        f"DynamicEventTable_{event_id}",
        (AbstractEventTable,),
        {
            "__tablename__": tablename,
            "__table_args__": {"extend_existing": True},
        },
    )
    _event_table_class_cache[event_id] = cast(Type[AbstractEventTable], cls)
    return _event_table_class_cache[event_id]


def get_world_bloom_table_class(event_id: int) -> Type[AbstractWorldBloomTable]:
    if event_id in _world_bloom_table_class_cache:
        return _world_bloom_table_class_cache[event_id]
    tablename = f"wl_{event_id}"
    cls = type(
        f"DynamicWorldBloomTable_{event_id}",
        (AbstractWorldBloomTable,),
        {
            "__tablename__": tablename,
            "__table_args__": {"extend_existing": True},
        },
    )
    _world_bloom_table_class_cache[event_id] = cast(Type[AbstractWorldBloomTable], cls)
    return _world_bloom_table_class_cache[event_id]


def get_event_names_table_class(event_id: int) -> Type[AbstractEventNamesTable]:
    if event_id in _event_names_table_class_cache:
        return _event_names_table_class_cache[event_id]
    tablename = f"event_{event_id}_names"
    cls = type(
        f"DynamicEventNamesTable_{event_id}",
        (AbstractEventNamesTable,),
        {
            "__tablename__": tablename,
            "__table_args__": {"extend_existing": True},
        },
    )
    _event_names_table_class_cache[event_id] = cast(Type[AbstractEventNamesTable], cls)
    return _event_names_table_class_cache[event_id]
