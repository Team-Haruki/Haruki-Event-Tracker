from typing import Type
from sqlalchemy import Column, Integer, String, BigInteger

from .base import Base


class AbstractEventTable(Base):
    __abstract__ = True
    timestamp = Column(BigInteger, primary_key=True)
    user_id = Column(String(30), primary_key=True)
    score = Column(Integer, nullable=False)
    rank = Column(Integer, nullable=False)


class AbstractWorldLinkTable(Base):
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
    tablename = f"event_{event_id}"

    class EventTable(AbstractEventTable):
        __tablename__ = tablename
        __table_args__ = {"extend_existing": True}

    return EventTable


def get_wl_table_class(event_id: int) -> Type[AbstractWorldLinkTable]:
    tablename = f"wl_{event_id}"

    class WorldLinkTable(AbstractWorldLinkTable):
        __tablename__ = tablename
        __table_args__ = {"extend_existing": True}

    return WorldLinkTable


def get_event_names_table_class(event_id: int) -> Type[AbstractEventNamesTable]:
    tablename = f"event_{event_id}_names"

    class EventNamesTable(AbstractEventNamesTable):
        __tablename__ = tablename
        __table_args__ = {"extend_existing": True}

    return EventNamesTable
