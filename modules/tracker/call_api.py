import orjson
import traceback
from typing import Optional
from aiohttp import ClientSession, ClientResponse

from modules.enums import SekaiServerRegion
from ..schema.call_api import Top100RankingResponse, BorderRankingResponse


class HarukiSekaiAPIClient:
    def __init__(self, api_endpoint: str) -> None:
        self.api_endpoint = api_endpoint
        self.session: Optional[ClientSession] = None
        self.headers = {"User-Agent": "Haruki Event Tracker / v1.0.0"}

    async def init(self) -> None:
        self.session = ClientSession()

    async def close(self) -> None:
        await self.session.close()

    async def _request(self, url: str) -> Optional[bytes]:
        async with self.session.get(url, headers=self.headers, timeout=20) as response:
            response.raise_for_status()
            if response.status == 200:
                return await response.read()
            else:
                return None

    async def get_top100(self, event_id: int, server: SekaiServerRegion) -> Optional[Top100RankingResponse]:
        try:
            url = f"{self.api_endpoint}/{server.value}/user/%user_id/event/{event_id}/ranking?rankingViewType=top100"
            response = await self._request(url)
            return Top100RankingResponse(**orjson.loads(response))
        except Exception:
            traceback.print_exc()
            return None

    async def get_border(self, event_id: int, server: SekaiServerRegion) -> Optional[BorderRankingResponse]:
        try:
            url = f"{self.api_endpoint}/{server.value}/event/{event_id}/ranking-border"
            response = await self._request(url)
            return BorderRankingResponse(**orjson.loads(response))
        except Exception:
            traceback.print_exc()
            return None
