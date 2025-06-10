import orjson
from redis.asyncio import StrictRedis
from typing import Optional, Dict, List, Union


class RedisClient(object):
    def __init__(self, host: str, port: int, password: Optional[str] = None) -> None:
        self._pool = StrictRedis(host=host, port=port, password=password, decode_responses=True)

    async def get(self, key: str) -> Optional[Union[Dict, List]]:
        raw = await self._pool.get(key)
        if raw is None:
            return None
        return orjson.loads(raw)

    async def set(self, key: str, value, ex: int = 300) -> None:
        await self._pool.set(key, orjson.dumps(value), ex=ex)

    async def delete(self, *keys: str) -> None:
        await self._pool.delete(*keys)

    async def keys(self, pattern: str) -> List[str]:
        return await self._pool.keys(pattern)

    async def clear_fastapi_cache(self, namespace: str) -> None:
        pattern = f"fastapi-cache:{namespace}:*"
        keys = await self._pool.keys(pattern)
        if keys:
            await self._pool.delete(*keys)

    async def close(self) -> None:
        await self._pool.close()
