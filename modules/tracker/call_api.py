from aiohttp import ClientSession, ClientResponse
from typing import Optional

from enums import SekaiServerRegion
from ..schema.sekai_api import Top100RankingResponse, BorderRankingResponse


class HarukiSekaiAPIClient:
    def __init__(self, api_endpoint: str) -> None:
        self.api_endpoint = api_endpoint
        self.session: Optional[ClientSession] = None
        self.headers = {
            "User-Agent": "Haruki Event Tracker / v1.0.0"
        }

    async def init(self) -> None:
        self.session = ClientSession()

    async def close(self) -> None:
        await self.session.close()

    async def _request(self, url: str) -> Optional[ClientResponse]:
        async with self.session.get(url, headers=self.headers) as response:
            if response.status == 200:
                return response
            else:
                return None

    async def get_top100(self, event_id: int, server: SekaiServerRegion) -> Optional[Top100RankingResponse]:
        url = f"{self.api_endpoint}/{server.value}/user/%user_id/event/{event_id}/ranking?rankingViewType=top100"
        response = await self._request(url)
        return Top100RankingResponse(**await response.json())

    async def get_border(self, event_id: int, server: SekaiServerRegion) -> Optional[BorderRankingResponse]:
        url = f"{self.api_endpoint}/{server.value}/event/{event_id}/ranking-border"
        response = await self._request(url)
        return BorderRankingResponse(**await response.json())
