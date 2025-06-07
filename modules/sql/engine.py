from sqlalchemy.schema import Table
from typing import Optional, Type, cast
from collections.abc import AsyncGenerator
from contextlib import asynccontextmanager
from sqlalchemy.ext.asyncio import create_async_engine, async_sessionmaker, AsyncSession

from .base import Base


class DatabaseEngine:
    def __init__(self, url_scheme: str) -> None:
        self._engine = create_async_engine(url_scheme, echo=False, future=True)
        self._session_maker = async_sessionmaker(self._engine, expire_on_commit=False)

    async def init_engine(self) -> None:
        async with self._engine.begin() as conn:
            await conn.run_sync(Base.metadata.create_all)

    async def create_tables(self, table_classes: list[Type[Base]]) -> None:
        async with self._engine.begin() as conn:
            for cls in table_classes:
                if cls is None:
                    continue
                table = cast(Table, cls.__table__)
                await conn.run_sync(table.create, checkfirst=True)

    @asynccontextmanager
    async def session(self) -> AsyncGenerator[Optional[AsyncSession]]:
        async with self._session_maker() as _session:
            yield _session

    async def shutdown_engine(self) -> None:
        await self._engine.dispose()
