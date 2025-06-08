from typing import List, Optional
from sqlalchemy import select, desc
from fastapi.responses import JSONResponse

from modules.sql.engine import DatabaseEngine
from modules.sql.tables import get_event_names_table_class
from modules.schema.api import (
    RecordedUserNameSchema,
    UserLatestRankingQueryResponseSchema,
    UserAllRankingDataQueryResponseSchema,
)


async def get_user_name_data(engine: DatabaseEngine, event_id: int, user_id: str) -> Optional[RecordedUserNameSchema]:
    table = get_event_names_table_class(event_id)
    async with engine.session() as session:
        stmt = select(table).where(table.user_id == user_id)
        result = await session.execute(stmt)
        row = result.scalar_one_or_none()
        if row:
            return RecordedUserNameSchema.model_validate(row)
    return None


async def fetch_ranking_rows(session, table, filters, latest_only: bool = False) -> List:
    stmt = select(table).where(*filters)
    stmt = stmt.order_by(desc(table.timestamp)).limit(1) if latest_only else stmt.order_by(table.timestamp.asc())
    result = await session.execute(stmt)
    return result.scalars().all()


async def generate_response(
    rows, schema_class, latest_only: bool, user_data
) -> JSONResponse:
    if not rows:
        return JSONResponse(content={"error": "not found"}, status_code=404)
    rank_data = schema_class.model_validate(rows[0]) if latest_only else [schema_class.model_validate(r) for r in rows]
    if latest_only:
        response = UserLatestRankingQueryResponseSchema(rank_data=rank_data, user_data=user_data)
    else:
        response = UserAllRankingDataQueryResponseSchema(rank_data=rank_data, user_data=user_data)
    return JSONResponse(content=response.model_dump())
